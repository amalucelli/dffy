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

use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use dffy_core::bstr::BString;
use dffy_core::diff::{Options, Text, Whitespace};
use dffy_core::git::{self, FileChange, Repo, Revisions, WhichCheckpoint};
use dffy_core::navigate::{self, Origin};
use dffy_core::view::Side;

use crate::config::{Context, DefaultDiff};
use crate::encode::{self, Compared, Detail, Files, Window};
use crate::navigator::client::{Helper, Helpers, Opened};
use crate::plugin::json::{Value, parse, string, write};
use crate::print::{Plan, Summary, Target};
use crate::render::Canvas;
use crate::review::model::At;
use crate::review::model::snapshot::Snapshot;
use crate::review::model::text::{lines, one_line};
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
    definitions finds where a name is defined at either version of the diff, in C, C++, Go, \
    Java, JavaScript, Python, Rust, TypeScript and the other languages it lists, and with \
    references: true the lines using it. \
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
      "since_checkpoint": {"type": ["string", "integer"], "description": "Instead of range: \"latest\" for what changed since the user's latest dffy checkpoint, or a checkpoint's number. A checkpoint is taken as the user hands their review to an agent, so this is the agent's own turn."},
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
      "since_checkpoint": {"type": ["string", "integer"], "description": "As for changed_files, instead of range."},
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
      "standing": {"type": "object", "additionalProperties": {"type": "string", "enum": ["posted", "draft", "local"]}, "description": "With session, where each comment on this page lives, by id: posted on the host, a draft of yours that posting sends, or local, kept here; empty without session."},
      "total_comments": {"type": "integer"},
      "next_cursor": {"type": ["string", "null"]}
    },
    "required": ["notice", "comments", "placed", "standing", "total_comments", "next_cursor"]
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
  "description": "Show a file in the dffy viewer the user has open in this repository, at a line when given, so the user sees what you're talking about. A file of the diff shows with its changes. Any other file in the repository opens whole, unchanged, at side's version, and the result has listed: false. It answers once the file shows. Fails, saying so, when no viewer runs here, no file is at that path on that side, or the viewer is still listing the diff's files (ask again).",
  "inputSchema": {
    "type": "object",
    "properties": {
      "path": {"type": "string", "description": "The file, relative to the repository root, as changed_files lists it, or any other file there."},
      "line": {"type": "integer", "minimum": 1, "description": "The line to show, 1-based, on side's numbering. Omit to show the file from its first change, or from its top when it has none."},
      "side": {"type": "string", "enum": ["old", "new"], "default": "new", "description": "Which version: line numbers are on it, and for a file outside the diff it's the version shown. new for the file as changed, old for it before."}
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
      "side": {"type": "string"},
      "listed": {"type": "boolean", "description": "Present, and false, when the diff doesn't list the file, which opened whole at side."}
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
},
{
  "name": "definitions",
  "title": "Where a name is defined",
  "description": "Find where a name (a function, macro, type, class, struct, enum, constant, method, field...) is defined, in Bash, C, C++, C#, Elixir, Go, Java, JavaScript, Kotlin, Lua, PHP, Python, Ruby, Rust, Scala, Swift, TypeScript or Zig code, at either version of a diff, without a language server: names are matched, not resolved, so a common name may have several candidates, nearest first (the same file, then the files it includes or imports, then the same folder or crate, then the rest). Each candidate says whether this diff changes its file (changed) and its lines (in_hunk). With path, the asking file and what it includes or imports rank first; without it, every definition of the name counts. With line and column of the use in path as well, the answer is much better: a local variable, a parameter or an import the use refers to comes first, and an imported name under another name finds what it names. The index of the version's definitions builds in the background from the first call: until it's done, partial is true and a definition may be missing; ask again for the full answer. A name with no definition found returns the lines using it, text_match true. references: true adds the lines using the name, past its definitions, comments and strings, in the first 200 files with one, paged: when next_cursor isn't null, call again with it as cursor and the same arguments. Read-only.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "name": {"type": "string", "description": "The name, as an identifier: letters, digits and _."},
      "path": {"type": "string", "description": "The file asking, relative to the repository root, in one of those languages."},
      "line": {"type": "integer", "minimum": 1, "description": "The line of path the name is used on, 1-based, in the version looked in."},
      "column": {"type": "integer", "minimum": 1, "description": "Where on line the name starts, 1-based, in bytes; without it, the name's first use in code on the line."},
      "side": {"type": "string", "enum": ["old", "new"], "default": "new", "description": "Which version of the diff to look in: new for the files as changed, old for them before."},
      "range": {"type": ["string", "array"], "items": {"type": "string"}, "description": "The diff, as for changed_files. Omit for uncommitted changes: new is then the working tree."},
      "since_checkpoint": {"type": ["string", "integer"], "description": "As for changed_files, instead of range."},
      "references": {"type": "boolean", "default": false, "description": "Add the lines using the name."},
      "calls_only": {"type": "boolean", "default": false, "description": "With references, only the lines calling the name."},
      "in_diff": {"type": "boolean", "default": false, "description": "With references, only the lines in files this diff changes."},
      "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 100, "description": "Lines using the name per page."},
      "cursor": {"type": "string", "description": "next_cursor from the previous page."}
    },
    "required": ["name"],
    "additionalProperties": false
  },
  "outputSchema": {
    "type": "object",
    "properties": {
      "name": {"type": "string"},
      "side": {"type": "string"},
      "candidates": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}, "start": {"type": "integer"}, "end": {"type": "integer"}, "kind": {"type": ["string", "null"]}, "container": {"type": ["string", "null"]}, "changed": {"type": "boolean"}, "in_hunk": {"type": ["boolean", "null"]}, "text_match": {"type": "boolean"}, "text": {"type": ["string", "null"]}}}},
      "total": {"type": "integer", "description": "Definitions found, beyond the 200 listed."},
      "progress": {"type": "object", "properties": {"indexed": {"type": "integer"}, "files": {"type": "integer"}}},
      "partial": {"type": "boolean", "description": "The index isn't complete: a definition may be missing."},
      "references": {"type": ["object", "null"], "properties": {"matches": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}, "line": {"type": "integer"}, "text": {"type": "string"}, "changed": {"type": "boolean"}}}}, "total": {"type": "integer"}, "files": {"type": "integer"}, "complete": {"type": "boolean"}}},
      "next_cursor": {"type": ["string", "null"]},
      "note": {"type": "string", "description": "Why the index is kept in memory rather than in the disk cache, when it is."}
    },
    "required": ["name", "side", "candidates", "total", "progress", "partial", "references", "next_cursor"]
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
        navigation: Navigation::default(),
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
    /// `definitions`' navigators and the uses it found.
    navigation: Navigation,
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
                .filter(|n| !n.is_empty() && !review::reads_as_you(n));
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
        self.navigation.tidy(Instant::now());
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
            "definitions" => self.definitions(&args),
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

    /// `since_checkpoint`: `"latest"`, or a checkpoint's number.
    fn checkpoint(&self) -> Result<Option<WhichCheckpoint>, String> {
        match self.get("since_checkpoint") {
            None => Ok(None),
            Some(Value::String(s)) if s == "latest" => Ok(Some(WhichCheckpoint::Latest)),
            Some(value) => (value.as_usize())
                .and_then(|n| u64::try_from(n).ok())
                .filter(|&n| n > 0)
                .map(|n| Some(WhichCheckpoint::Number(n)))
                .ok_or_else(|| {
                    "since_checkpoint must be \"latest\" or a checkpoint's number".to_owned()
                }),
        }
    }

    /// `definitions`' `line` and `column` of the use, 1-based, as the
    /// core's 0-based place.
    fn place(&self) -> Result<Option<navigate::Use>, String> {
        let most = u32::MAX as usize;
        match (
            self.number("line", 0, [1, most])?,
            self.number("column", 0, [1, most])?,
        ) {
            (0, 0) => Ok(None),
            (0, _) => Err("column needs line".to_owned()),
            (line, column) => Ok(Some(navigate::Use {
                line: line - 1,
                column: column.checked_sub(1),
            })),
        }
    }

    /// `definitions`' `name`: an identifier.
    fn name(&self) -> Result<String, String> {
        let name = (self.string("name")?).ok_or("name is required: the name to look up")?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(format!(
                "{name:?} isn't a name: letters, digits and _ only, as an identifier is"
            ));
        }
        Ok(name.to_owned())
    }

    /// `side`: the diff's `new` version, or its `old` one.
    fn side(&self) -> Result<Side, String> {
        match self.string("side")? {
            None | Some("new") => Ok(Side::New),
            Some("old") => Ok(Side::Old),
            Some(_) => Err("side must be \"old\" or \"new\"".to_owned()),
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
    /// `since_checkpoint`, in place of `revisions`.
    since: Option<WhichCheckpoint>,
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

    /// Resolves `revisions`, or the checkpoint `since` names, and `paths`
    /// (relative to the repository root) against the repository as it is
    /// now.
    fn range(
        &mut self,
        revisions: Vec<String>,
        since: Option<WhichCheckpoint>,
        paths: &[String],
    ) -> Result<Range, String> {
        if revisions.iter().any(|r| r == "-") {
            return Err("range takes revisions; '-' (a diff on stdin) isn't one".to_owned());
        }
        if since.is_some() && !revisions.is_empty() {
            return Err(
                "since_checkpoint compares a checkpoint with the working tree; give it or range, \
                 not both"
                    .to_owned(),
            );
        }
        let repo = self.repo()?;
        let root = repo
            .workdir()
            .map_or_else(|| self.base.clone(), Path::to_owned);
        let target = Revisions {
            unstaged: revisions.is_empty()
                && since.is_none()
                && self.ctx.config.default_diff == DefaultDiff::Unstaged,
            revs: revisions.clone(),
            since_checkpoint: since,
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
        Ok(Range {
            revisions,
            since,
            plan,
        })
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
        args.only(&[
            "range",
            "since_checkpoint",
            "paths",
            "syntax",
            "counts",
            "limit",
            "cursor",
        ])?;
        let revisions = args.strings("range", true)?;
        let since = args.checkpoint()?;
        let paths = args.strings("paths", false)?;
        let syntax = args.bool("syntax", false)?;
        let counts = args.bool("counts", true)?;
        let limit = args.number("limit", 200, [1, 5000])?;
        let cursor = args.cursor()?;
        if syntax && !counts {
            return Err("syntax: true reads every file; it needs counts left on".to_owned());
        }
        let range = self.range(revisions, since, &paths)?;
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
            "since_checkpoint",
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
        let since = args.checkpoint()?;
        let syntax = args.bool("syntax", false)?;
        let context = args.number("context", 3, [0, 10_000])?;
        let max_lines = args.number("max_lines", 400, [1, 20_000])?;
        let comments = args.comments()?;
        let cursor = args.cursor()?;
        let range = self.range(revisions, since, &[])?;
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
        let range = self.range(revisions, None, &paths)?;
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
        let listed = (self.range(revisions, None, &paths))
            .and_then(|range| Ok((self.changes(&range)?, range)));
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
        let placed: HashMap<String, At> = placed.into_iter().flatten().collect();
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
        structured.push_str(r#","standing":{"#);
        if session {
            let by_id: HashMap<&str, &review::Comment> =
                (file.comments.iter()).map(|c| (c.id.as_str(), c)).collect();
            for (i, comment) in page.iter().enumerate() {
                let root = (comment.in_reply_to.as_deref()).and_then(|r| by_id.get(r).copied());
                let standing = review::model::standing(comment, root, true);
                if i > 0 {
                    structured.push(',');
                }
                string(&mut structured, &comment.id);
                structured.push(':');
                string(&mut structured, standing.name().unwrap_or_default());
            }
        }
        structured.push('}');
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
            let _ = writeln!(text, "notice: {}", one_line(notice));
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
        let review_head = if session {
            review::read_session(&self.repo()?)?.map(|pointer| pointer.head)
        } else {
            None
        };
        let review = (&file, review_head.as_deref());
        let prompt = crate::print::thread_prompt(self.repo().ok(), review, id, self.ctx)
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
        match range.since {
            Some(WhichCheckpoint::Latest) => {
                return "the changes since the latest checkpoint".to_owned();
            }
            Some(WhichCheckpoint::Number(n)) => return format!("the changes since checkpoint {n}"),
            None => {}
        }
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
        let unlisted = if result.get("listed") == Some(&Value::Bool(false)) {
            let side = result.get("side").and_then(Value::as_str).unwrap_or("new");
            format!(" It isn't in the diff, so it shows whole at the {side} side.")
        } else {
            String::new()
        };
        let text = match result.get("line").and_then(Value::as_usize) {
            Some(line) => format!("The viewer shows {path} at line {line}.{unlisted}"),
            None => format!("The viewer shows {path}.{unlisted}"),
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

/// `definitions`' state across calls: the navigator helper of the
/// repository (docs/navigate.md), which holds the navigators and builds
/// their index on 2 threads of its own, and the searches it runs, kept for
/// later pages and continued by the next call while unfinished. The server
/// runs beside the user's viewer and agent, so the helper is closed, giving
/// its memory back, once unused for a while.
struct Navigation {
    helper: Helpers,
    /// The generation of the helper the searches were made in.
    generation: u64,
    /// The last `definitions` call.
    used: Option<Instant>,
    /// Most recent last, at most [`SEARCHES`].
    searches: Vec<Search>,
    /// The text searches for names with no definition found, most recent
    /// last, at most [`SEARCHES`].
    texts: Vec<TextSearch>,
    /// How long a call searches before it answers with what it found.
    budget: Duration,
    /// How long the helper goes unused before it's closed.
    idle: Duration,
}

impl Default for Navigation {
    fn default() -> Self {
        Self {
            helper: Helpers::default(),
            generation: 0,
            used: None,
            searches: Vec::new(),
            texts: Vec::new(),
            budget: BUDGET,
            idle: IDLE,
        }
    }
}

/// What a search is of: the navigator's fingerprint, tagged with its
/// helper's generation, its id, the asking file and the name.
type SearchKey = (u64, String, BString, String);

/// A search for a name's uses in the helper and the lines it found so far.
struct Search {
    key: SearchKey,
    /// Which uses it keeps.
    filter: navigate::Filter,
    /// Its id in the helper.
    id: u64,
    hits: Vec<navigate::Reference>,
    /// Files with a use so far.
    files: usize,
    done: bool,
    /// The index was complete through every step, so the definitions' own
    /// lines are left out.
    indexed: bool,
}

/// A text search for a name with no definition found, and the lines it
/// found so far.
struct TextSearch {
    key: SearchKey,
    id: u64,
    matches: Vec<navigate::Candidate>,
    done: bool,
}

/// How long the helper goes unused before it's closed, with its index.
const IDLE: Duration = Duration::from_secs(10 * 60);

/// How long a call searches for uses or text matches before it answers.
const BUDGET: Duration = Duration::from_secs(2);

/// Threads the helper builds the index on.
const BUILD_THREADS: usize = 2;

/// Searches for uses kept for paging or to continue.
const SEARCHES: usize = 8;

/// Text matches `definitions` returns for a name with no definition found.
const TEXT_MATCHES: usize = 50;

/// Why `definitions` can't run: its index lives in the disk cache.
const NO_CACHE: &str = "definitions keeps its index in the disk cache, which is off here \
    (cache = false or --no-cache)";

impl Navigation {
    /// Closes the helper once unused for [`Navigation::idle`], with what
    /// it searched. Checked as each call starts, so nothing runs between
    /// calls but the helper's build.
    fn tidy(&mut self, now: Instant) {
        if self
            .used
            .is_some_and(|at| now.saturating_duration_since(at) >= self.idle)
        {
            self.helper.close();
            self.searches.clear();
            self.texts.clear();
            self.used = None;
        }
    }
}

impl Drop for Navigation {
    fn drop(&mut self) {
        self.helper.close();
    }
}

/// When a call's budget is spent.
struct Deadline(Instant);

impl Deadline {
    fn start(budget: Duration) -> Self {
        Self(Instant::now() + budget)
    }

    /// What's left of it, for a step the helper runs.
    fn left(&self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }
}

/// `definitions`, through the navigator helper (docs/mcp.md).
impl Server<'_> {
    /// The navigator of `side` of the diff between `snapshots`, opened in
    /// the helper, which keeps the one it has while the core says it still
    /// fits the diff. A helper that isn't running is started, once again
    /// when it's gone, unless the helpers keep exiting. Its fingerprint is
    /// tagged with the helper's generation, so no search of a helper gone
    /// is asked of another.
    fn navigator(
        &mut self,
        repo: &Repo,
        snapshots: [git::Snapshot; 2],
        side: Side,
    ) -> Result<(Helper, Opened), String> {
        let cache = self.ctx.cache.as_ref().ok_or(NO_CACHE)?;
        let root = repo
            .workdir()
            .or_else(|| repo.git_dir())
            .ok_or("definitions needs a repository")?;
        let nav = &mut self.navigation;
        nav.used = Some(Instant::now());
        let start = |_| Helper::start(root, cache, BUILD_THREADS, None);
        let (mut generation, mut helper) = nav.helper.get(None, start)?;
        let mut opened = helper.open(snapshots, side, Whitespace::Exact);
        if let Err(refused) = &opened
            && refused.helper_gone()
        {
            let gone = Some((generation, refused.message.as_str()));
            (generation, helper) = nav.helper.get(gone, start)?;
            opened = helper.open(snapshots, side, Whitespace::Exact);
        }
        if generation != nav.generation {
            nav.generation = generation;
            nav.searches.clear();
            nav.texts.clear();
        }
        let mut opened = opened.map_err(|refused| {
            if refused.helper_gone() {
                format!("definitions unavailable: {}", refused.message)
            } else {
                refused.message
            }
        })?;
        opened.fingerprint = format!("{generation}/{}", opened.fingerprint);
        Ok((helper, opened))
    }

    fn definitions(&mut self, args: &Args) -> Result<Output, String> {
        args.only(&[
            "name",
            "path",
            "line",
            "column",
            "side",
            "range",
            "since_checkpoint",
            "references",
            "calls_only",
            "in_diff",
            "limit",
            "cursor",
        ])?;
        let name = args.name()?;
        let path = args.string("path")?.map(str::to_owned);
        let side = args.side()?;
        let with_references = args.bool("references", false)?;
        let limit = args.number("limit", 100, [1, 1000])?;
        let cursor = args.cursor()?;
        let revisions = args.strings("range", true)?;
        let since = args.checkpoint()?;
        let range = self.range(revisions, since, &[])?;
        let snapshots = (range.plan.spec)
            .snapshots(range.plan.reverse)
            .ok_or("definitions needs git's versions of the files; two paths compared aren't")?;
        if self.ctx.cache.is_none() {
            return Err(NO_CACHE.to_owned());
        }
        let repo = range.plan.repo.clone();
        let asking = path
            .as_deref()
            .map(|path| asking_file(&range.plan, path))
            .transpose()?;
        let at = args.place()?;
        if at.is_some() && asking.is_none() {
            return Err("line and column are of the use in path: give path too".to_owned());
        }
        let (helper, opened) = self.navigator(&repo, snapshots, side)?;
        let origins: Vec<(BString, String)> = asking.map_or_else(
            || {
                (opened.languages.iter())
                    .map(|language| (BString::default(), language.clone()))
                    .collect()
            },
            |origin| vec![(origin.path, origin.language.name().to_owned())],
        );
        let origin = origins
            .first()
            .ok_or("this version has no files of a language definitions reads")?;
        let deadline = Deadline::start(self.navigation.budget);
        let found = found_definitions(
            (&helper, &opened),
            (&name, &origins, at),
            &mut self.navigation.texts,
            &deadline,
        )?;
        let progress = found.progress;
        let lines = candidate_lines(&repo, &found.candidates);

        let mut h = request_hash("definitions", &range);
        let filter = navigate::Filter {
            calls_only: args.bool("calls_only", false)?,
            in_diff: args.bool("in_diff", false)?,
        };
        (
            &name,
            &path,
            side == Side::Old,
            &opened.fingerprint,
            limit,
            filter,
        )
            .hash(&mut h);
        let fingerprint = h.finish();
        let start = cursor.map_or(Ok((0, 0)), |c| c.check(fingerprint))?.0;
        let wanted = start.saturating_add(limit);
        let search = if with_references {
            let found = (&helper, &opened, progress);
            Some(self.references(found, origin, (&name, filter), wanted, &deadline)?)
        } else {
            None
        };
        let page = search.map(|search| Page::of(search, start..wanted, fingerprint));
        let note = helper
            .progress(opened.navigator)
            .ok()
            .and_then(|built| built.note);
        let answer = Answer {
            name: &name,
            side: if side == Side::Old { "old" } else { "new" },
            found: &found,
            lines: &lines,
            progress,
            page,
            note: note.as_deref(),
        };
        Ok(Output {
            structured: answer.object(),
            text: answer.text(),
        })
    }
}

impl<'a> Page<'a> {
    /// The lines of `search` at `wanted`, so far, with the cursor to the
    /// next page of request `fingerprint` while more may come.
    fn of(search: &'a Search, wanted: std::ops::Range<usize>, fingerprint: u64) -> Self {
        let end = wanted.end.min(search.hits.len());
        let rows = wanted.start.min(end)..end;
        let capped = search.files >= navigate::MAX_REFERENCE_FILES;
        let more = rows.end < search.hits.len() || (!search.done && !capped);
        let next = more.then(|| {
            Cursor {
                fingerprint,
                at: (rows.end.max(wanted.start), 0),
            }
            .encode()
        });
        Self {
            rows,
            hits: &search.hits,
            files: search.files,
            complete: search.done && search.indexed,
            next,
        }
    }
}

/// A page of the uses found so far.
struct Page<'a> {
    rows: std::ops::Range<usize>,
    hits: &'a [navigate::Reference],
    /// Files with a use, so far when not complete.
    files: usize,
    /// Every file was searched, with the index whole.
    complete: bool,
    next: Option<String>,
}

/// What `definitions` found, to write as its result.
struct Answer<'a> {
    name: &'a str,
    side: &'static str,
    found: &'a FoundDefinitions,
    lines: &'a [Option<String>],
    progress: navigate::Progress,
    /// The page of uses, when they were asked for.
    page: Option<Page<'a>>,
    /// Why the helper keeps the index in memory, when it does.
    note: Option<&'a str>,
}

impl Answer<'_> {
    fn object(&self) -> String {
        let Self {
            name,
            side,
            found,
            lines,
            progress,
            page,
            note,
        } = self;
        let mut s = String::from(r#"{"name":"#);
        string(&mut s, name);
        s.push_str(r#","side":"#);
        string(&mut s, side);
        s.push_str(r#","candidates":["#);
        for (i, c) in found.candidates.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            encode::candidate(c, lines.get(i).and_then(Option::as_deref), &mut s);
        }
        let partial = !progress.complete();
        let complete = !partial && found.complete;
        let _ = write!(
            s,
            r#"],"total":{},"progress":{{"indexed":{},"files":{}}},"partial":{partial},"complete":{complete},"references":"#,
            found.total, progress.done, progress.total
        );
        match page {
            Some(page) => {
                s.push_str(r#"{"matches":["#);
                for (i, hit) in page.hits[page.rows.clone()].iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    encode::reference(hit, &mut s);
                }
                let _ = write!(
                    s,
                    r#"],"total":{},"files":{},"complete":{}}}"#,
                    page.hits.len(),
                    page.files,
                    page.complete
                );
            }
            None => s.push_str("null"),
        }
        s.push_str(r#","next_cursor":"#);
        optional_string(&mut s, page.as_ref().and_then(|p| p.next.as_deref()));
        if let Some(note) = note {
            s.push_str(r#","note":"#);
            string(&mut s, note);
        }
        s.push('}');
        s
    }

    fn text(&self) -> String {
        let Self {
            name,
            side,
            found,
            lines,
            progress,
            page,
            note,
        } = self;
        let FoundDefinitions {
            candidates,
            total,
            text_match,
            complete,
            ..
        } = found;
        let mut text = match (text_match, complete) {
            (true, true) => format!(
                "No definition of {name} found in the {side} version; {} lines using it:",
                candidates.len()
            ),
            (true, false) => format!(
                "No definition of {name} found in the {side} version; {} lines using it, \
                 from the files searched in the time a call has:",
                candidates.len()
            ),
            (false, _) => {
                let n = if *total == 1 { "" } else { "s" };
                format!("{total} definition{n} of {name} in the {side} version:")
            }
        };
        if !progress.complete() {
            let _ = write!(
                text,
                " (the index is partial: {} of {} files so far; ask again for the rest)",
                progress.done, progress.total
            );
        }
        if let Some(note) = note {
            let _ = write!(text, "\nNote: {note}.");
        }
        text.push('\n');
        for (i, c) in candidates.iter().enumerate() {
            let what = c.kind.map_or("text match", |k| k.name());
            let _ = write!(text, "{}:{}", c.path, c.start + 1);
            if c.end > c.start {
                let _ = write!(text, "-{}", c.end + 1);
            }
            let _ = write!(text, " {what}");
            if c.in_hunk == Some(true) {
                text.push_str(", changed in this diff");
            }
            if let Some(line) = lines.get(i).and_then(Option::as_deref) {
                let _ = write!(text, ": {line}");
            }
            text.push('\n');
        }
        if let Some(page) = page {
            let capped = page.files >= navigate::MAX_REFERENCE_FILES;
            let so_far = if page.complete { "" } else { " so far" };
            let listed = if capped {
                format!(", the first {} listed", navigate::MAX_REFERENCE_FILES)
            } else {
                String::new()
            };
            let _ = writeln!(
                text,
                "{} lines use it in {} files{so_far}{listed}:",
                page.hits.len(),
                page.files,
            );
            for hit in &page.hits[page.rows.clone()] {
                let _ = write!(
                    text,
                    "{}:{}: {}",
                    hit.candidate.path,
                    hit.candidate.start + 1,
                    hit.text
                );
                if hit.more > 0 {
                    let _ = write!(text, " (+{} more in this file)", hit.more);
                }
                text.push('\n');
            }
            if !page.complete && page.next.is_none() {
                text.push_str(if capped {
                    "The files past those listed weren't searched.\n"
                } else {
                    "The index was partial: some of these lines may be definitions.\n"
                });
            }
        }
        next_line(
            page.as_ref().and_then(|p| p.next.as_deref()),
            "references",
            &mut text,
        );
        text
    }
}

impl Server<'_> {
    /// The search for the uses of `name` seen from `origin`, continued in
    /// the helper until `deadline`, at least a file, until it has a line
    /// past the `wanted`, has found every file it lists, or is done. A
    /// search that finished on a partial index is made again once the
    /// index is whole.
    fn references(
        &mut self,
        (helper, opened, progress): (&Helper, &Opened, navigate::Progress),
        (path, language): &(BString, String),
        (name, filter): (&str, navigate::Filter),
        wanted: usize,
        deadline: &Deadline,
    ) -> Result<&Search, String> {
        let searches = &mut self.navigation.searches;
        let key = (
            opened.navigator,
            opened.fingerprint.clone(),
            path.clone(),
            name.to_owned(),
        );
        let kept = (searches.iter())
            .position(|s| s.key == key && s.filter == filter)
            .map(|at| searches.remove(at));
        let kept = kept.filter(|s| s.indexed || !(progress.complete() && s.done));
        let mut search = match kept {
            Some(search) => search,
            None => Search {
                id: (helper)
                    .start_search(
                        opened.navigator,
                        "references",
                        name,
                        (path.as_slice(), language),
                        (0, filter),
                    )
                    .map_err(|e| e.message)?,
                key,
                filter,
                hits: Vec::new(),
                files: 0,
                done: false,
                indexed: true,
            },
        };
        let capped = |s: &Search| s.files >= navigate::MAX_REFERENCE_FILES;
        if !search.done && !capped(&search) && search.hits.len() <= wanted {
            let budget = (deadline.left(), Some(wanted + 1), false);
            match helper.step_references(search.id, budget, None) {
                Ok(step) => {
                    search.hits.extend(step.hits);
                    search.files = step.files;
                    search.done = step.done;
                    search.indexed &= step.indexed;
                }
                Err(refused) => {
                    helper.close_search(search.id);
                    return Err(format!(
                        "the search for its uses stopped: {}",
                        refused.message
                    ));
                }
            }
        }
        searches.push(search);
        if searches.len() > SEARCHES {
            let gone = searches.remove(0);
            helper.close_search(gone.id);
        }
        searches.last().ok_or_else(|| "no search".to_owned())
    }
}

/// The asking file `path`, a file of `plan`'s repository in a language with
/// definitions.
fn asking_file(plan: &Plan, path: &str) -> Result<Origin, String> {
    let wanted = plan.relative(Path::new(path)).map_err(message)?;
    let language = (crate::grammars().detect(&wanted, b""))
        .filter(|l| l.has_tags())
        .ok_or_else(|| format!("definitions doesn't read {path}'s language"))?;
    Ok(Origin {
        path: wanted,
        language: language.clone(),
    })
}

/// What `definitions` found of a name.
struct FoundDefinitions {
    candidates: Vec<navigate::Candidate>,
    /// Definitions in all, past those listed.
    total: usize,
    /// None was found: the candidates are lines using the name.
    text_match: bool,
    /// The text search went through every file, or found its cap.
    complete: bool,
    /// How far the version's index was built when it answered.
    progress: navigate::Progress,
}

/// The definitions of `name` seen from `origins`, as the helper looks them
/// up: one asking file's, nearest first, or with no asking file one origin
/// per language, merged by path and cut to the first [`MAX_LISTED`]. With
/// none found, the lines using it: a text search in the helper kept in
/// `texts` and gone on with until `deadline`, at least a file a call, until
/// it has [`TEXT_MATCHES`] lines or every file is searched.
fn found_definitions(
    (helper, opened): (&Helper, &Opened),
    (name, origins, at): (&str, &[(BString, String)], Option<navigate::Use>),
    texts: &mut Vec<TextSearch>,
    deadline: &Deadline,
) -> Result<FoundDefinitions, String> {
    let mut candidates = Vec::new();
    let mut total = 0;
    let mut progress = opened.progress;
    for (path, language) in origins {
        let found = helper
            .lookup(
                opened.navigator,
                name,
                (path.as_slice(), language),
                at,
                None,
            )
            .map_err(|e| e.message)?;
        total += found.total;
        progress = found.progress;
        candidates.extend(found.candidates);
    }
    if origins.len() > 1 {
        candidates.sort_by(|a, b| (&a.path, a.start).cmp(&(&b.path, b.start)));
        candidates.truncate(MAX_LISTED);
    }
    match origins.first() {
        Some((path, language)) if candidates.is_empty() => {
            let key = (
                opened.navigator,
                opened.fingerprint.clone(),
                path.clone(),
                name.to_owned(),
            );
            let kept = (texts.iter().position(|t| t.key == key)).map(|at| texts.remove(at));
            let mut text = match kept {
                Some(text) => text,
                None => TextSearch {
                    id: helper
                        .start_search(
                            opened.navigator,
                            "text",
                            name,
                            (path.as_slice(), language),
                            (TEXT_MATCHES, navigate::Filter::default()),
                        )
                        .map_err(|e| e.message)?,
                    key,
                    matches: Vec::new(),
                    done: false,
                },
            };
            if !text.done {
                let step = (helper.step_text(text.id, deadline.left(), None))
                    .map_err(|e| format!("the text search stopped: {}", e.message))?;
                text.matches.extend(step.hits);
                text.done = step.done;
            }
            let found = FoundDefinitions {
                total: text.matches.len(),
                complete: text.done,
                candidates: text.matches.clone(),
                text_match: true,
                progress,
            };
            texts.push(text);
            if texts.len() > SEARCHES {
                let gone = texts.remove(0);
                helper.close_search(gone.id);
            }
            Ok(found)
        }
        _ => Ok(FoundDefinitions {
            candidates,
            total,
            text_match: false,
            complete: true,
            progress,
        }),
    }
}

/// Definitions `definitions` lists at most, as a lookup does from one
/// asking file.
const MAX_LISTED: usize = 200;

/// The first line of each candidate, trimmed; `None` where its file can't
/// be read.
fn candidate_lines(repo: &Repo, candidates: &[navigate::Candidate]) -> Vec<Option<String>> {
    let mut read: Option<(&BString, Option<Text>)> = None;
    candidates
        .iter()
        .map(|c| {
            if read.as_ref().is_none_or(|(path, _)| **path != c.path) {
                read = Some((
                    &c.path,
                    repo.load(c.path.as_ref(), &c.side).ok().map(Text::new),
                ));
            }
            let text = read.as_ref()?.1.as_ref()?;
            (c.start < text.len()).then(|| {
                String::from_utf8_lossy(text.line(c.start))
                    .trim()
                    .to_owned()
            })
        })
        .collect()
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
                        let i = next.fetch_add(1, Ordering::Relaxed);
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
    text.push_str(&one_line(field(file, "path").as_str().unwrap_or_default()));
    if status == "renamed" {
        let from = one_line(field(file, "old_path").as_str().unwrap_or_default());
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
            text.push_str(&one_line(heading));
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
            let code = field(line, "text").as_str().unwrap_or_default();
            text.push_str(&one_line(code.strip_suffix('\r').unwrap_or(code)));
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
        let id = one_line(field(annotation, "id").as_str().unwrap_or_default());
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
            let said = |key| field(comment, key).as_str().unwrap_or_default();
            let (author, created) = (review::who(said("author")), one_line(said("created")));
            let mut lines = lines(said("text").trim_end());
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

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .status()
            .expect("git runs")
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A C repository whose `inc/f.h` defines `frob`, used by four files,
    /// over two commits, with an uncommitted edit; and the defaults with a
    /// disk cache in `cache`.
    fn navigating(dir: &Path, cache: &Path) -> Ctx {
        crate::navigator::client::use_program(crate::navigator::built_program());
        git(dir, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(dir.join("inc")).expect("mkdir");
        std::fs::write(
            dir.join("inc/f.h"),
            "static inline int frob(void) { return 0; }\n",
        )
        .expect("write");
        for name in ["a.c", "b.c", "c.c", "d.c"] {
            let text = format!(
                "#include <f.h>\nint {}(void) {{ return frob(); }}\n",
                &name[..1]
            );
            std::fs::write(dir.join(name), text).expect("write");
        }
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "first"]);
        std::fs::write(
            dir.join("inc/f.h"),
            "static inline int frob(void) { return 1; }\n",
        )
        .expect("write");
        git(dir, &["commit", "-q", "-am", "second"]);
        std::fs::write(dir.join("a.c"), "int a(void) { return frob() + 1; }\n").expect("write");
        Ctx {
            config: crate::config::Config::default(),
            sources: crate::config::Sources::default(),
            last_used: Vec::new(),
            tip_shown: false,
            context_lines: 3,
            config_path: None,
            config_warnings: Vec::new(),
            theme: crate::theme::Theme::default_theme(),
            term: crate::output::Term {
                stdout_tty: false,
                stderr_tty: false,
                width: None,
            },
            color_out: false,
            color_err: false,
            cache: Some(dffy_core::cache::Cache::new(cache.to_owned(), 1 << 24)),
            review_name: std::sync::OnceLock::new(),
            review: std::sync::OnceLock::new(),
            comments: None,
        }
    }

    fn server<'a>(dir: &Path, ctx: &'a Ctx) -> Server<'a> {
        Server {
            repo: Repo::discover(dir).ok(),
            comments: None,
            base: dir.to_owned(),
            ctx,
            legacy: false,
            client: None,
            navigation: Navigation::default(),
        }
    }

    /// `tool`'s structured result for `args`, which must not fail.
    fn tool(server: &mut Server, tool: &str, args: &str) -> Value {
        let call = format!(r#"{{"name":"{tool}","arguments":{args}}}"#);
        let params = parse(call.as_bytes()).expect("json");
        let members = server.call(Some(&params)).expect("a tool result");
        let result = parse(format!("{{{members}}}").as_bytes()).expect("json");
        assert_eq!(
            result.get("isError"),
            Some(&Value::Bool(false)),
            "{members}"
        );
        result
            .get("structuredContent")
            .cloned()
            .expect("structured")
    }

    /// `definitions` once the index is whole.
    fn indexed(server: &mut Server, args: &str) -> Value {
        for _ in 0..400 {
            let found = tool(server, "definitions", args);
            if found.get("partial") == Some(&Value::Bool(false)) {
                return found;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("the index was never built");
    }

    /// A call searches for uses for its budget, at least a file, and
    /// answers with what it found: `complete` false and a cursor that
    /// continues the same search on the next call, until every file is.
    /// `in_diff` keeps the uses in the files the diff changes, and
    /// `calls_only` those calling the name: each a search of its own.
    #[test]
    fn uses_are_filtered_to_the_diff_and_to_calls() {
        let (dir, cache) = (
            tempfile::tempdir().expect("tempdir"),
            tempfile::tempdir().expect("tempdir"),
        );
        let ctx = navigating(dir.path(), cache.path());
        let mut server = server(dir.path(), &ctx);
        indexed(&mut server, r#"{"name":"frob","path":"b.c"}"#);
        let files = |server: &mut Server, filters: &str| {
            let ask = format!(r#"{{"name":"frob","path":"b.c","references":true{filters}}}"#);
            loop {
                let found = tool(server, "definitions", &ask);
                let refs = found.get("references").cloned().unwrap_or(Value::Null);
                if refs.get("complete") == Some(&Value::Bool(true)) {
                    let paths: Vec<String> = (refs.get("matches").and_then(Value::as_array))
                        .expect("matches")
                        .iter()
                        .filter_map(|m| m.get("path").and_then(Value::as_str).map(str::to_owned))
                        .collect();
                    return paths;
                }
            }
        };
        assert_eq!(files(&mut server, ""), ["b.c", "a.c", "c.c", "d.c"]);
        assert_eq!(files(&mut server, r#","in_diff":true"#), ["a.c"]);
        assert_eq!(
            files(&mut server, r#","calls_only":true"#),
            ["b.c", "a.c", "c.c", "d.c"]
        );
    }

    /// Where the helper can't keep the index in the cache's folder, a full
    /// disk, the answer's text says it's kept in memory.
    #[test]
    fn the_answer_says_when_the_index_is_kept_in_memory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocked = tempfile::NamedTempFile::new().expect("a file where the cache would be");
        let ctx = navigating(dir.path(), blocked.path());
        let mut server = server(dir.path(), &ctx);
        indexed(&mut server, r#"{"name":"frob","path":"b.c"}"#);
        let call = r#"{"name":"definitions","arguments":{"name":"frob","path":"b.c"}}"#;
        let members =
            (server.call(Some(&parse(call.as_bytes()).expect("json")))).expect("a tool result");
        assert!(
            members.contains("the definitions index is kept in memory"),
            "{members}"
        );
        let result = parse(format!("{{{members}}}").as_bytes()).expect("json");
        let note = (result.get("structuredContent")).and_then(|c| c.get("note"));
        assert!(
            note.and_then(Value::as_str)
                .is_some_and(|n| n.contains("kept in memory")),
            "{members}"
        );
    }

    #[test]
    fn a_search_for_uses_answers_within_its_budget_and_goes_on_next_call() {
        let (dir, cache) = (
            tempfile::tempdir().expect("tempdir"),
            tempfile::tempdir().expect("tempdir"),
        );
        let ctx = navigating(dir.path(), cache.path());
        let mut server = server(dir.path(), &ctx);
        indexed(&mut server, r#"{"name":"frob","path":"b.c"}"#);
        server.navigation.budget = Duration::ZERO;
        let ask = |cursor: Option<&str>| {
            let cursor = cursor.map_or(String::new(), |c| format!(r#","cursor":"{c}""#));
            format!(r#"{{"name":"frob","path":"b.c","references":true{cursor}}}"#)
        };
        let refs = |found: &Value, key: &str| {
            (found.get("references").and_then(|r| r.get(key)).cloned()).unwrap_or(Value::Null)
        };
        let first = tool(&mut server, "definitions", &ask(None));
        assert_eq!(
            refs(&first, "complete"),
            Value::Bool(false),
            "one file a call"
        );
        let mut lines = refs(&first, "matches").as_array().map_or(0, <[Value]>::len);
        let mut cursor = first
            .get("next_cursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut calls = 1;
        while let Some(at) = cursor {
            let next = tool(&mut server, "definitions", &ask(Some(&at)));
            lines += refs(&next, "matches").as_array().map_or(0, <[Value]>::len);
            cursor = next
                .get("next_cursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            calls += 1;
            if cursor.is_none() {
                assert_eq!(refs(&next, "complete"), Value::Bool(true));
                assert_eq!(refs(&next, "files").as_usize(), Some(4));
            }
        }
        assert!(calls > 4, "{calls} calls");
        assert_eq!(lines, 4, "each use once, across the calls");
    }

    /// A name nothing defines gets its text matches from the files searched
    /// in the call's budget, at least one, `complete` false; the next call
    /// with the same arguments goes on with the same search until every
    /// file is.
    #[test]
    fn a_text_search_answers_within_its_budget_and_goes_on_next_call() {
        let (dir, cache) = (
            tempfile::tempdir().expect("tempdir"),
            tempfile::tempdir().expect("tempdir"),
        );
        let ctx = navigating(dir.path(), cache.path());
        for name in ["a.c", "b.c", "c.c", "d.c"] {
            let text = format!("/* uses zap */\nint {}x;\n", &name[..1]);
            std::fs::write(dir.path().join(name), text).expect("write");
        }
        let mut server = server(dir.path(), &ctx);
        let ask = r#"{"name":"zap","path":"b.c"}"#;
        indexed(&mut server, r#"{"name":"frob","path":"b.c"}"#);
        server.navigation.budget = Duration::ZERO;
        let first = tool(&mut server, "definitions", ask);
        assert_eq!(first.get("complete"), Some(&Value::Bool(false)));
        let count = |found: &Value| {
            found
                .get("candidates")
                .and_then(Value::as_array)
                .map_or(0, <[Value]>::len)
        };
        assert!(count(&first) <= 1, "one file a call");
        let mut calls = 1;
        loop {
            let next = tool(&mut server, "definitions", ask);
            calls += 1;
            if next.get("complete") == Some(&Value::Bool(true)) {
                assert_eq!(count(&next), 4, "every file's line, once");
                break;
            }
            assert!(calls < 20, "never completes");
        }
        assert!(calls > 4, "{calls} calls");
    }

    /// `definitions` asks the navigator helper, one for every version asked
    /// about, and closes it, with what it searched, once unused for a
    /// while: checked as the next call of any tool starts. The next ask
    /// starts another.
    #[test]
    fn the_helper_serves_every_version_and_goes_when_idle() {
        let (dir, cache) = (
            tempfile::tempdir().expect("tempdir"),
            tempfile::tempdir().expect("tempdir"),
        );
        let ctx = navigating(dir.path(), cache.path());
        let mut server = server(dir.path(), &ctx);
        for args in [
            r#"{"name":"frob","path":"b.c","range":"HEAD~1..HEAD"}"#,
            r#"{"name":"frob","path":"b.c","range":"HEAD~1..HEAD","side":"old"}"#,
            r#"{"name":"frob","path":"b.c","references":true}"#,
        ] {
            // From the whole index: on a loaded machine a header's parse on
            // the spot may run out of the time it gets.
            let found = indexed(&mut server, args);
            assert_eq!(
                found.get("total").and_then(Value::as_usize),
                Some(1),
                "{args}"
            );
        }
        assert!(server.navigation.helper.current().is_some());
        assert_eq!(server.navigation.searches.len(), 1);

        let idle = server.navigation.idle + Duration::from_secs(1);
        server.navigation.used = Instant::now().checked_sub(idle);
        tool(&mut server, "changed_files", "{}");
        assert!(
            server.navigation.helper.current().is_none(),
            "an idle helper goes"
        );
        assert!(server.navigation.searches.is_empty());
        let found = indexed(&mut server, r#"{"name":"frob","path":"b.c"}"#);
        assert_eq!(found.get("total").and_then(Value::as_usize), Some(1));
    }

    /// A helper killed is started again on the next call, a search of the
    /// one gone dropped; after three starts within a minute the call says
    /// the helper keeps exiting.
    #[test]
    fn a_helper_that_dies_is_started_again_a_few_times() {
        let (dir, cache) = (
            tempfile::tempdir().expect("tempdir"),
            tempfile::tempdir().expect("tempdir"),
        );
        let ctx = navigating(dir.path(), cache.path());
        let mut server = server(dir.path(), &ctx);
        let args = r#"{"name":"frob","path":"b.c","references":true}"#;
        let kill = |server: &Server| {
            let (_, helper) = server.navigation.helper.current().expect("runs");
            let pid = helper.pid().expect("a process");
            rustix::process::kill_process(pid, rustix::process::Signal::KILL).ok();
            let start = Instant::now();
            while helper.pid().is_some() {
                assert!(start.elapsed() < Duration::from_secs(10), "it never went");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let before = indexed(&mut server, args);
        let first = server.navigation.generation;
        kill(&server);
        assert_eq!(
            indexed(&mut server, args),
            before,
            "another helper's answer"
        );
        assert_ne!(server.navigation.generation, first);
        assert_eq!(
            server.navigation.searches.len(),
            1,
            "the old search dropped"
        );
        kill(&server);
        indexed(&mut server, args);
        kill(&server);
        let call = r#"{"name":"definitions","arguments":{"name":"frob","path":"b.c"}}"#;
        let params = parse(call.as_bytes()).expect("json");
        let members = server.call(Some(&params)).expect("a tool result");
        assert!(
            members.contains("definitions unavailable: the helper keeps exiting")
                && members.contains(r#""isError":true"#),
            "{members}"
        );
    }

    /// A lookup the helper dies on is a tool error saying so, and the next
    /// name is looked up in another.
    #[cfg(debug_assertions)]
    #[test]
    fn a_name_the_helper_dies_on_leaves_the_next_one_working() {
        let (dir, cache) = (
            tempfile::tempdir().expect("tempdir"),
            tempfile::tempdir().expect("tempdir"),
        );
        let ctx = navigating(dir.path(), cache.path());
        let mut server = server(dir.path(), &ctx);
        let name = crate::navigator::DEADLY;
        let call =
            format!(r#"{{"name":"definitions","arguments":{{"name":"{name}","path":"b.c"}}}}"#);
        let params = parse(call.as_bytes()).expect("json");
        let members = server.call(Some(&params)).expect("a tool result");
        assert!(
            members.contains("the navigator helper exited")
                && members.contains(r#""isError":true"#),
            "{members}"
        );
        let found = indexed(&mut server, r#"{"name":"frob","path":"b.c"}"#);
        assert_eq!(found.get("total").and_then(Value::as_usize), Some(1));
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
                "viewer_selection",
                "definitions"
            ]
        );
    }
}

//! The control API: a running viewer driven from outside, by `dffy ctl`,
//! the MCP server or any program that speaks the protocol (docs/control.md
//! is the contract). dffyd serves it on a socket of its own next to the
//! watch broker's; viewers register there after their first frame, and
//! dffyd hands each request for a view to its viewer and the answer back.
//! The viewer stays the only owner of what it shows: requests become
//! [`viewer::Command`]s on its main loop, and a comment it's asked to add
//! goes through the same write as one typed into it.
//!
//! Every message is one line of JSON. A connection opens with the version
//! handshake; then requests carry an `id` and a `method`, answers the same
//! `id` and a `result` or an `error`, and notifications a `method` without
//! an `id`. Fields a side doesn't know are ignored, so either can add some
//! without a new version. Unix only: the transport is a Unix socket.
//!
//! To add a message:
//! 1. name it in [`METHODS`] (or [`EVENTS`] for a notification) and give it
//!    a definition in [`SCHEMA`];
//! 2. route it in [`server`]: dffyd's own, or forwarded to the view;
//! 3. for a view's, parse it into a [`viewer::Command`], encode its result
//!    in [`viewer`] (with [`crate::encode`] for what the core model has),
//!    and answer it in the viewer's `tui::app::control`;
//! 4. document it in docs/control.md.

pub(crate) mod client;
pub(crate) mod server;
pub(crate) mod viewer;

use std::io::{self, BufRead, Read as _};
use std::path::{Path, PathBuf};

use crate::plugin::json::{self, Value};

/// The protocol's version, in the handshake and the socket's name: dffyds
/// of other versions serve sockets of their own.
pub(crate) const VERSION: usize = 1;

/// Every request dffyd answers, itself or through a view.
pub(crate) const METHODS: [&str; 8] = [
    "views/list",
    "views/register",
    "subscribe",
    "view/state",
    "view/selection",
    "view/open",
    "view/focusThread",
    "comments/add",
];

/// The notifications a client can subscribe to.
pub(crate) const EVENTS: [&str; 2] = ["selection/changed", "view/changed"];

/// Lines longer than this are refused rather than buffered.
const MAX_LINE: u64 = 1 << 20;

/// The control socket in dffyd's runtime directory.
pub(crate) fn socket_path(dir: &Path) -> PathBuf {
    dir.join(format!("control-v{VERSION}.sock"))
}

/// The lock the dffyd serving [`socket_path`] holds, so one serves it.
pub(crate) fn lock_path(dir: &Path) -> PathBuf {
    dir.join(format!("control-v{VERSION}.lock"))
}

/// The error codes, as [`SCHEMA`] lists them.
const CODES: [&str; 9] = [
    "parse_error",
    "invalid_request",
    "unknown_method",
    "invalid_params",
    "no_view",
    "viewer_gone",
    "not_found",
    "failed",
    "unsupported_version",
];

/// An error code read off the wire; one of another version is `failed`.
pub(crate) fn code(name: &str) -> &'static str {
    CODES.into_iter().find(|c| *c == name).unwrap_or("failed")
}

/// Why a request failed, as the `error` member of its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fault {
    pub code: &'static str,
    pub message: String,
}

impl Fault {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_params", message)
    }
}

/// Reads the next line's JSON, `None` at the end. A line that isn't JSON
/// is `Ok(Some(Err(..)))`, for the reader to answer and go on.
pub(crate) fn read(
    from: &mut impl BufRead,
    line: &mut Vec<u8>,
) -> io::Result<Option<Result<Value, String>>> {
    loop {
        line.clear();
        let n = from.by_ref().take(MAX_LINE).read_until(b'\n', line)?;
        if n == 0 {
            return Ok(None);
        }
        if line.last() != Some(&b'\n') && n as u64 == MAX_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a control message over 1MB",
            ));
        }
        if !line.iter().all(u8::is_ascii_whitespace) {
            return Ok(Some(json::parse(line)));
        }
    }
}

/// `value` as one line.
pub(crate) fn line(value: &Value) -> String {
    let mut out = String::new();
    json::write(&mut out, value);
    out.push('\n');
    out
}

pub(crate) fn object(fields: Vec<(&str, Value)>) -> Value {
    Value::Object(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

pub(crate) fn text(s: impl Into<String>) -> Value {
    Value::String(s.into())
}

#[expect(
    clippy::cast_precision_loss,
    reason = "ids, pids and line numbers are far below 2^53, where f64 is exact"
)]
pub(crate) fn number(n: u64) -> Value {
    Value::Number(n as f64)
}

/// The answer to request `id`.
pub(crate) fn answer(id: &Value, result: Result<Value, Fault>) -> String {
    line(&match result {
        Ok(result) => object(vec![("id", id.clone()), ("result", result)]),
        Err(fault) => object(vec![("id", id.clone()), ("error", fault_value(&fault))]),
    })
}

pub(crate) fn fault_value(fault: &Fault) -> Value {
    object(vec![
        ("code", text(fault.code)),
        ("message", text(fault.message.clone())),
    ])
}

pub(crate) fn notification(method: &str, params: Value) -> String {
    line(&object(vec![("method", text(method)), ("params", params)]))
}

/// The JSON Schema of every message, which `dffy ctl schema` prints.
pub(crate) const SCHEMA: &str = r##"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://github.com/amalucelli/dffy/docs/control.md",
  "title": "dffy control protocol, version 1",
  "description": "Newline-delimited JSON over the Unix socket control-v1.sock in dffyd's runtime directory. A connection opens with hello and welcome; then requests, answers and notifications. Fields not listed here are ignored. Lines are 1-based.",
  "$defs": {
    "hello": {"description": "The client's first line.", "type": "object", "properties": {"version": {"const": 1}, "client": {"type": "string", "description": "The client's name, which comments it adds are by; up to 64 characters are kept."}}, "required": ["version"]},
    "welcome": {"description": "dffyd's answer to hello.", "type": "object", "properties": {"version": {"const": 1}, "server": {"const": "dffyd"}, "build": {"type": "string"}, "capabilities": {"type": "object", "properties": {"methods": {"type": "array", "items": {"type": "string"}}, "events": {"type": "array", "items": {"type": "string"}}}, "required": ["methods", "events"]}}, "required": ["version", "capabilities"]},
    "refused": {"description": "dffyd's answer to a hello of another version, before it hangs up.", "type": "object", "properties": {"error": {"$ref": "#/$defs/error"}, "versions": {"type": "array", "items": {"type": "integer"}}}, "required": ["error", "versions"]},
    "request": {"type": "object", "properties": {"id": {"type": ["integer", "string"]}, "method": {"type": "string"}, "params": {"type": "object"}, "client": {"type": "string", "description": "On a request dffyd hands a viewer: the asking client's name from its hello, when it gave one."}}, "required": ["id", "method"]},
    "answer": {"type": "object", "properties": {"id": {"type": ["integer", "string"]}, "result": {"type": "object"}, "error": {"$ref": "#/$defs/error"}}, "required": ["id"], "oneOf": [{"required": ["result"]}, {"required": ["error"]}]},
    "notification": {"type": "object", "properties": {"method": {"type": "string"}, "params": {"type": "object"}}, "required": ["method", "params"], "not": {"required": ["id"]}},
    "error": {"type": "object", "properties": {"code": {"enum": ["parse_error", "invalid_request", "unknown_method", "invalid_params", "no_view", "viewer_gone", "not_found", "failed", "unsupported_version"]}, "message": {"type": "string"}}, "required": ["code", "message"]},
    "side": {"enum": ["old", "new"]},
    "lines": {"type": "object", "description": "A row's line on each side, null on a side it hasn't.", "properties": {"old": {"type": ["integer", "null"]}, "new": {"type": ["integer", "null"]}}, "required": ["old", "new"]},
    "view": {"type": "object", "properties": {"view": {"type": "integer"}, "pid": {"type": "integer"}, "worktree": {"type": "string"}, "repository": {"type": "string", "description": "The git directory the repository's worktrees share."}, "diff": {"type": "string", "description": "What the view compares, as its title says."}}, "required": ["view", "pid", "worktree", "repository", "diff"]},
    "state": {"type": "object", "properties": {"view": {"type": "integer"}, "path": {"type": ["string", "null"]}, "old_path": {"type": ["string", "null"]}, "top": {"$ref": "#/$defs/lines"}, "files": {"type": "integer"}, "listing": {"type": "boolean"}, "layout": {"enum": ["split", "unified"]}, "diff": {"type": "string"}, "revisions": {"type": "array", "items": {"type": "string"}}}, "required": ["view", "path", "top", "files", "listing", "layout", "diff", "revisions"]},
    "selection": {"type": ["object", "null"], "properties": {"path": {"type": "string"}, "side": {"$ref": "#/$defs/side"}, "start": {"type": "integer"}, "end": {"type": "integer"}, "text": {"type": "string"}}, "required": ["path", "side", "start", "end", "text"]},
    "views/list": {"description": "The registered viewers.", "properties": {"params": {"type": "object"}, "result": {"type": "object", "properties": {"views": {"type": "array", "items": {"$ref": "#/$defs/view"}}}, "required": ["views"]}}},
    "views/register": {"description": "A viewer joins, once per connection; closing the connection leaves.", "properties": {"params": {"type": "object", "properties": {"pid": {"type": "integer"}, "worktree": {"type": "string"}, "repository": {"type": "string"}, "diff": {"type": "string"}}, "required": ["pid", "worktree", "repository", "diff"]}, "result": {"type": "object", "properties": {"view": {"type": "integer"}}, "required": ["view"]}}},
    "subscribe": {"description": "Pushes a view's events to this connection until it closes: each once with what's there now, then on each change.", "properties": {"params": {"type": "object", "properties": {"view": {"type": "integer"}, "events": {"type": "array", "items": {"enum": ["selection/changed", "view/changed"]}}}, "required": ["view", "events"]}, "result": {"type": "object", "properties": {"view": {"type": "integer"}, "events": {"type": "array", "items": {"type": "string"}}}, "required": ["view", "events"]}}},
    "view/state": {"description": "The file shown, its top row's lines and what the view compares.", "properties": {"params": {"type": "object", "properties": {"view": {"type": "integer"}}, "required": ["view"]}, "result": {"$ref": "#/$defs/state"}}},
    "view/selection": {"description": "The lines selected in the diff, null when none are.", "properties": {"params": {"type": "object", "properties": {"view": {"type": "integer"}}, "required": ["view"]}, "result": {"type": "object", "properties": {"view": {"type": "integer"}, "selection": {"$ref": "#/$defs/selection"}}, "required": ["view", "selection"]}}},
    "view/open": {"description": "Shows a file of the diff, at a line when given. Answered once it shows.", "properties": {"params": {"type": "object", "properties": {"view": {"type": "integer"}, "path": {"type": "string", "description": "Relative to the working tree's root."}, "line": {"type": "integer", "minimum": 1}, "side": {"$ref": "#/$defs/side", "default": "new"}}, "required": ["view", "path"]}, "result": {"type": "object", "properties": {"view": {"type": "integer"}, "path": {"type": "string"}, "line": {"type": ["integer", "null"]}, "side": {"$ref": "#/$defs/side"}}, "required": ["view", "path", "line", "side"]}}},
    "view/focusThread": {"description": "Shows and focuses a comment thread, named by any of its comments' ids.", "properties": {"params": {"type": "object", "properties": {"view": {"type": "integer"}, "id": {"type": "string"}}, "required": ["view", "id"]}, "result": {"type": "object", "properties": {"view": {"type": "integer"}, "thread": {"type": "string"}, "path": {"type": "string"}, "focused": {"type": "boolean", "description": "false while its file's diff or the thread's place is on its way: it's focused once there."}}, "required": ["view", "thread", "path", "focused"]}}},
    "comments/add": {"description": "Adds a comment on a line, as the viewer's c would, to the comments file the view shows; the view shows it.", "properties": {"params": {"type": "object", "properties": {"view": {"type": "integer"}, "path": {"type": "string"}, "line": {"type": "integer", "minimum": 1}, "side": {"$ref": "#/$defs/side", "default": "new"}, "text": {"type": "string", "minLength": 1}}, "required": ["view", "path", "line", "text"]}, "result": {"type": "object", "properties": {"view": {"type": "integer"}, "id": {"type": "string"}, "path": {"type": "string"}, "line": {"type": "integer"}, "side": {"$ref": "#/$defs/side"}}, "required": ["view", "id", "path", "line", "side"]}}},
    "selection/changed": {"description": "Pushed to subscribers, debounced.", "properties": {"params": {"type": "object", "properties": {"view": {"type": "integer"}, "selection": {"$ref": "#/$defs/selection"}}, "required": ["view", "selection"]}}},
    "view/changed": {"description": "Pushed to subscribers, debounced: the state when the file or the top row moved.", "properties": {"params": {"$ref": "#/$defs/state"}}},
    "view/closed": {"description": "Pushed to subscribers when their view's viewer leaves.", "properties": {"params": {"type": "object", "properties": {"view": {"type": "integer"}}, "required": ["view"]}}},
    "subscriptions": {"description": "dffyd to a viewer: the events someone subscribes to now.", "properties": {"params": {"type": "object", "properties": {"events": {"type": "array", "items": {"type": "string"}}}, "required": ["events"]}}},
    "$/cancelRequest": {"description": "dffyd to a viewer: the client that asked request id left before its answer, so it's not to be done.", "properties": {"params": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}}}
  }
}
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every message dffyd and viewers exchange has a definition, and the
    /// schema is valid JSON.
    #[test]
    fn the_schema_defines_every_message() {
        let schema = json::parse(SCHEMA.as_bytes()).expect("JSON");
        let defs = schema.get("$defs").expect("$defs");
        for name in METHODS.iter().chain(&EVENTS).chain(&[
            "hello",
            "welcome",
            "view/closed",
            "subscriptions",
            "$/cancelRequest",
        ]) {
            assert!(defs.get(name).is_some(), "{name}");
        }
    }

    #[test]
    fn a_line_over_the_cap_is_refused_and_blank_lines_are_skipped() {
        let mut buf = Vec::new();
        let mut input = &b"\n  \n{\"version\":1}\n"[..];
        assert_eq!(
            read(&mut input, &mut buf).expect("read"),
            Some(Ok(object(vec![("version", number(1))])))
        );
        assert_eq!(read(&mut input, &mut buf).expect("read"), None);
        let long = vec![b'x'; usize::try_from(MAX_LINE).expect("fits") + 1];
        assert!(read(&mut &long[..], &mut buf).is_err());
        assert!(matches!(read(&mut &b"{\n"[..], &mut buf), Ok(Some(Err(_)))));
    }
}

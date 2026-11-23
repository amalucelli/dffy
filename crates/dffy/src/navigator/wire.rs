//! The navigator protocol's values as JSON, written and read by the same
//! functions on both sides (docs/navigate.md), so the helper and its hosts
//! can't disagree on a field.

use std::fmt::Write as _;

use dffy_core::bstr::BString;
use dffy_core::diff::Whitespace;
use dffy_core::git::{self, Mode, ObjectId, Snapshot};
use dffy_core::highlight::DefinitionKind;
use dffy_core::navigate::{Candidate, Filter, Progress, Rank, Reference, Use};
use dffy_core::view::Side;

use crate::plugin::json::{Value, string};

/// A field of `value` that must be there.
pub(crate) fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value, String> {
    value.get(key).ok_or_else(|| format!("no {key}"))
}

pub(crate) fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    field(value, key)?
        .as_str()
        .ok_or_else(|| format!("{key} isn't a string"))
}

pub(crate) fn number(value: &Value, key: &str) -> Result<usize, String> {
    field(value, key)?
        .as_usize()
        .ok_or_else(|| format!("{key} isn't a whole number"))
}

pub(crate) fn flag(value: &Value, key: &str) -> Result<bool, String> {
    match field(value, key)? {
        Value::Bool(b) => Ok(*b),
        _ => Err(format!("{key} isn't true or false")),
    }
}

fn id(value: &Value, key: &str) -> Result<ObjectId, String> {
    ObjectId::from_hex(text(value, key)?.as_bytes()).map_err(|_| format!("{key} isn't an id"))
}

/// A use's place in the asking file, `,"at":{"line","column"}`: 0-based,
/// the column a byte of the line, or null for the name's first use in
/// code on it. Nothing without a place.
pub(crate) fn at(at: Option<Use>, s: &mut String) {
    let Some(at) = at else {
        return;
    };
    let _ = write!(s, r#","at":{{"line":{},"column":"#, at.line);
    match at.column {
        Some(column) => {
            let _ = write!(s, "{column}");
        }
        None => s.push_str("null"),
    }
    s.push('}');
}

/// A uses search's `filter`, `,"calls_only","in_diff"` after a comma, each
/// only when it's set.
pub(crate) fn filter(filter: Filter, s: &mut String) {
    if filter.calls_only {
        s.push_str(r#","calls_only":true"#);
    }
    if filter.in_diff {
        s.push_str(r#","in_diff":true"#);
    }
}

/// The filter of `params`: each flag false when it's left out.
pub(crate) fn read_filter(params: &Value) -> Result<Filter, String> {
    let set = |key| match params.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(_) => flag(params, key),
    };
    Ok(Filter {
        calls_only: set("calls_only")?,
        in_diff: set("in_diff")?,
    })
}

/// The `at` of `params`, when it has one.
pub(crate) fn read_use(params: &Value) -> Result<Option<Use>, String> {
    let at = match params.get("at") {
        None | Some(Value::Null) => return Ok(None),
        Some(at) => at,
    };
    let column = match at.get("column") {
        None | Some(Value::Null) => None,
        Some(_) => Some(number(at, "column")?),
    };
    Ok(Some(Use {
        line: number(at, "line")?,
        column,
    }))
}

/// `{"kind":"tree","id":…}`, `{"kind":"index"}` or `{"kind":"worktree"}`.
pub(crate) fn snapshot(at: Snapshot, s: &mut String) {
    match at {
        Snapshot::Tree(id) => {
            let _ = write!(s, r#"{{"kind":"tree","id":"{id}"}}"#);
        }
        Snapshot::Index => s.push_str(r#"{"kind":"index"}"#),
        Snapshot::Worktree => s.push_str(r#"{"kind":"worktree"}"#),
    }
}

pub(crate) fn read_snapshot(value: &Value) -> Result<Snapshot, String> {
    match text(value, "kind")? {
        "tree" => Ok(Snapshot::Tree(id(value, "id")?)),
        "index" => Ok(Snapshot::Index),
        "worktree" => Ok(Snapshot::Worktree),
        other => Err(format!("no snapshot kind {other}")),
    }
}

pub(crate) fn side(side: Side) -> &'static str {
    match side {
        Side::Old => "old",
        Side::New => "new",
    }
}

pub(crate) fn read_side(value: &Value, key: &str) -> Result<Side, String> {
    match text(value, key)? {
        "old" => Ok(Side::Old),
        "new" => Ok(Side::New),
        other => Err(format!("{key} is old or new, not {other}")),
    }
}

pub(crate) fn whitespace(w: Whitespace) -> &'static str {
    match w {
        Whitespace::Exact => "exact",
        Whitespace::IgnoreAll => "ignore_all",
        Whitespace::IgnoreChange => "ignore_change",
    }
}

pub(crate) fn read_whitespace(value: &Value, key: &str) -> Result<Whitespace, String> {
    match text(value, key)? {
        "exact" => Ok(Whitespace::Exact),
        "ignore_all" => Ok(Whitespace::IgnoreAll),
        "ignore_change" => Ok(Whitespace::IgnoreChange),
        other => Err(format!("no whitespace mode {other}")),
    }
}

/// The asking file: its path and its language's name.
pub(crate) fn origin(path: &[u8], language: &str, s: &mut String) {
    s.push_str(r#"{"path":"#);
    string(s, &String::from_utf8_lossy(path));
    s.push_str(r#","language":"#);
    string(s, language);
    s.push('}');
}

pub(crate) fn read_origin(value: &Value) -> Result<(BString, String), String> {
    Ok((
        text(value, "path")?.into(),
        text(value, "language")?.to_owned(),
    ))
}

pub(crate) fn progress(p: Progress, s: &mut String) {
    let _ = write!(s, r#"{{"done":{},"total":{}}}"#, p.done, p.total);
}

pub(crate) fn read_progress(value: &Value) -> Result<Progress, String> {
    Ok(Progress {
        done: number(value, "done")?,
        total: number(value, "total")?,
    })
}

fn mode(m: Mode) -> &'static str {
    match m {
        Mode::File => "file",
        Mode::Executable => "executable",
        Mode::Symlink => "symlink",
        Mode::Gitlink => "gitlink",
    }
}

fn read_mode(value: &Value) -> Result<Mode, String> {
    match text(value, "mode")? {
        "file" => Ok(Mode::File),
        "executable" => Ok(Mode::Executable),
        "symlink" => Ok(Mode::Symlink),
        "gitlink" => Ok(Mode::Gitlink),
        other => Err(format!("no mode {other}")),
    }
}

/// Where a file is at a navigator's side: a blob, or the working-tree
/// file; a file under one of two paths compared outside git, which no
/// navigator lists, reads back as written all the same.
fn file(at: &git::Side, s: &mut String) {
    match at {
        git::Side::Object { id, mode: m } => {
            let _ = write!(s, r#"{{"kind":"blob","id":"{id}","mode":"{}"}}"#, mode(*m));
        }
        git::Side::Worktree { mode: m, id } => {
            let _ = write!(s, r#"{{"kind":"worktree","mode":"{}","id":"#, mode(*m));
            match id {
                Some(id) => {
                    let _ = write!(s, r#""{id}""#);
                }
                None => s.push_str("null"),
            }
            s.push('}');
        }
        git::Side::Path { mode: m, root } => {
            let root = match root {
                git::PathRoot::Old => "old",
                git::PathRoot::New => "new",
            };
            let _ = write!(
                s,
                r#"{{"kind":"path","mode":"{}","root":"{root}"}}"#,
                mode(*m)
            );
        }
    }
}

fn read_file(value: &Value) -> Result<git::Side, String> {
    match text(value, "kind")? {
        "blob" => Ok(git::Side::Object {
            id: id(value, "id")?,
            mode: read_mode(value)?,
        }),
        "worktree" => Ok(git::Side::Worktree {
            mode: read_mode(value)?,
            id: match field(value, "id")? {
                Value::Null => None,
                _ => Some(id(value, "id")?),
            },
        }),
        "path" => Ok(git::Side::Path {
            mode: read_mode(value)?,
            root: match text(value, "root")? {
                "old" => git::PathRoot::Old,
                "new" => git::PathRoot::New,
                other => return Err(format!("no path root {other}")),
            },
        }),
        other => Err(format!("no file kind {other}")),
    }
}

fn rank(r: Rank) -> &'static str {
    match r {
        Rank::SameFile => "same_file",
        Rank::Imported => "imported",
        Rank::Near => "near",
        Rank::Elsewhere => "elsewhere",
    }
}

fn read_rank(value: &Value) -> Result<Rank, String> {
    match text(value, "rank")? {
        "same_file" => Ok(Rank::SameFile),
        "imported" => Ok(Rank::Imported),
        "near" => Ok(Rank::Near),
        "elsewhere" => Ok(Rank::Elsewhere),
        other => Err(format!("no rank {other}")),
    }
}

/// A definition found, or a line using the name (`kind` null), its lines
/// 0-based as the core has them.
pub(crate) fn candidate(c: &Candidate, s: &mut String) {
    s.push('{');
    candidate_fields(c, s);
    s.push('}');
}

fn candidate_fields(c: &Candidate, s: &mut String) {
    s.push_str(r#""path":"#);
    string(s, &String::from_utf8_lossy(&c.path));
    s.push_str(r#","file":"#);
    file(&c.side, s);
    s.push_str(r#","kind":"#);
    match c.kind {
        Some(kind) => string(s, kind.name()),
        None => s.push_str("null"),
    }
    let _ = write!(s, r#","start":{},"end":{},"container":"#, c.start, c.end);
    match &c.container {
        Some(container) => string(s, container),
        None => s.push_str("null"),
    }
    let _ = write!(
        s,
        r#","rank":"{}","changed":{},"in_hunk":"#,
        rank(c.rank),
        c.changed
    );
    match c.in_hunk {
        Some(b) => {
            let _ = write!(s, "{b}");
        }
        None => s.push_str("null"),
    }
}

pub(crate) fn read_candidate(value: &Value) -> Result<Candidate, String> {
    Ok(Candidate {
        path: text(value, "path")?.into(),
        side: read_file(field(value, "file")?)?,
        kind: match field(value, "kind")? {
            Value::Null => None,
            Value::String(name) => {
                Some(DefinitionKind::from_name(name).ok_or_else(|| format!("no kind {name}"))?)
            }
            _ => return Err("kind isn't a string".to_owned()),
        },
        start: number(value, "start")?,
        end: number(value, "end")?,
        container: match field(value, "container")? {
            Value::Null => None,
            other => Some(other.as_str().ok_or("container isn't a string")?.to_owned()),
        },
        rank: read_rank(value)?,
        changed: flag(value, "changed")?,
        in_hunk: match field(value, "in_hunk")? {
            Value::Null => None,
            Value::Bool(b) => Some(*b),
            _ => return Err("in_hunk isn't true, false or null".to_owned()),
        },
    })
}

/// A line using the name: a candidate without a kind, with the line's text
/// and, on a file's last line listed, its lines past those.
pub(crate) fn reference(r: &Reference, s: &mut String) {
    s.push('{');
    candidate_fields(&r.candidate, s);
    s.push_str(r#","text":"#);
    string(s, &r.text);
    let _ = write!(s, r#","more":{}}}"#, r.more);
}

pub(crate) fn read_reference(value: &Value) -> Result<Reference, String> {
    Ok(Reference {
        candidate: read_candidate(value)?,
        text: text(value, "text")?.to_owned(),
        more: number(value, "more")?,
    })
}

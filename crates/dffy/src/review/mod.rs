//! Review comments: the comments file that the viewer draws and
//! integrations keep in step with a code host (docs/comments.md is the
//! contract). This module owns the format, the operations that write the
//! file under a lock, and placing a comment on a side's text by its anchor.
//! Nothing here knows about the viewer.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::hash::{BuildHasher as _, RandomState};
use std::io::{self, Write as _};
use std::ops::Range;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use dffy_core::diff::{Hunk, LineKind, Text};
use dffy_core::git::{Head, Repo};

use crate::plugin::json::{self, Value};
use crate::render::{Canvas, escape};
use crate::theme::Role;

const VERSION: usize = 1;

/// Lines of context an anchor keeps on each side of the commented lines.
const CONTEXT: usize = 2;

/// How far from its recorded first line a moved anchor is looked for.
const NEARBY: usize = 1000;

type Fields = Vec<(String, Value)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Side {
    Old,
    #[default]
    New,
}

impl Side {
    fn name(self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::New => "new",
        }
    }
}

/// A line's number on each side, 0-based (the file's are 1-based); `None`
/// on a side the line isn't on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct LineNumbers {
    pub old: Option<usize>,
    pub new: Option<usize>,
}

impl LineNumbers {
    pub(crate) fn on(self, side: Side) -> Option<usize> {
        match side {
            Side::Old => self.old,
            Side::New => self.new,
        }
    }
}

/// The commented lines' text and up to [`CONTEXT`] lines around them, each
/// without its `\n` and decoded lossily.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Anchor {
    pub before: Vec<String>,
    pub lines: Vec<String>,
    pub after: Vec<String>,
}

/// A comment as the file holds it. Line comments have `start`, `end` and
/// `anchor`; file comments and replies have none of them. A general
/// comment, on the review rather than a file, has no path either.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Comment {
    pub id: String,
    /// Empty for a general comment.
    pub path: String,
    pub old_path: Option<String>,
    /// The side of `end`; only meaningful for a line comment.
    pub side: Side,
    /// The side of `start`, when it isn't `side`.
    pub start_side: Option<Side>,
    pub start: Option<LineNumbers>,
    pub end: Option<LineNumbers>,
    pub text: String,
    pub suggestion: Option<String>,
    /// Empty means you.
    pub author: String,
    /// RFC 3339.
    pub created: String,
    /// The thread's first comment, for a reply.
    pub in_reply_to: Option<String>,
    pub resolved: bool,
    /// The file's `ref`: owned by an integration, and set once the comment
    /// is posted, which makes its text read-only.
    pub reference: Option<String>,
    pub anchor: Option<Anchor>,
    pub distance: Option<usize>,
    /// The comment on its host, set by an integration.
    pub url: Option<String>,
    /// The host marks the thread outdated: its lines changed since it was
    /// written. Set on a thread's first comment.
    pub outdated: bool,
    /// What a general comment is, in the integration's word: `description`,
    /// `review`, `comment`, or one dffy doesn't know.
    pub kind: Option<String>,
    /// A word the integration gives a general comment, shown as given,
    /// such as a review's verdict.
    pub label: Option<String>,
    /// The branch a local comment was written on; the thread's, on its
    /// first comment and copied to its replies. `None` shows on every
    /// branch.
    pub branch: Option<String>,
    /// Set by an integration: on the host, visible only to you until the
    /// review is submitted.
    pub pending: bool,
    /// Set by an integration: why the host refused to take the comment.
    pub post_error: Option<String>,
    /// Fields dffy doesn't know, in file order, written back as read.
    pub extra: Fields,
}

/// The whole comments file.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Review {
    pub notice: Option<String>,
    /// The review's summary as you're writing it, before it's posted.
    pub summary: Option<String>,
    pub comments: Vec<Comment>,
    /// Top-level fields dffy doesn't know, written back as read.
    pub extra: Fields,
}

/// The comment an operation names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Id(String),
    Ref(String),
}

/// What a `set` changes; `None` leaves a field alone, and `Some(None)`
/// removes an optional one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[expect(
    clippy::option_option,
    reason = "the JSON's three states: left out, null, and a value"
)]
pub(crate) struct Change {
    pub text: Option<String>,
    pub resolved: Option<bool>,
    pub reference: Option<Option<String>>,
    pub anchor: Option<Anchor>,
    /// Only to empty: the comment becomes yours.
    pub author: Option<String>,
    pub suggestion: Option<Option<String>>,
    /// The whole thread's, whichever of its comments is named.
    pub branch: Option<Option<String>>,
    pub pending: Option<bool>,
    pub post_error: Option<Option<String>>,
}

/// One of `dffy comments apply`'s operations.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Op {
    /// `comment.id` and `comment.in_reply_to` are ignored: dffy assigns the
    /// id, and a reply names any comment of its thread in `reply_to`. An
    /// empty `created` becomes the time of the write.
    Add {
        comment: Box<Comment>,
        reply_to: Option<Target>,
    },
    Set {
        target: Target,
        change: Change,
    },
    /// The comment's fields as given, a `ref` among them. They're merged
    /// into the comment with that ref field by field, so imports work on
    /// the JSON rather than a [`Comment`] that can't tell a field left out.
    Upsert {
        fields: Fields,
        reply_to: Option<Target>,
    },
    Rm(Target),
    Notice(Option<String>),
    Summary(Option<String>),
}

impl Op {
    fn name(&self) -> &'static str {
        match self {
            Self::Add { .. } => "add",
            Self::Set { .. } => "set",
            Self::Upsert { .. } => "upsert",
            Self::Rm(_) => "rm",
            Self::Notice(_) => "notice",
            Self::Summary(_) => "summary",
        }
    }
}

/// The local review's file, and the session pointer's, under the git
/// directory every worktree shares.
const LOCAL: &str = "dffy/comments.json";
const SESSION: &str = "dffy/session";

/// The git directory every worktree shares, canonical: in a linked
/// worktree git gives it relative to the worktree's own.
fn shared_dir(repo: &Repo) -> Option<PathBuf> {
    let dir = repo.common_dir()?;
    Some(dir.canonicalize().unwrap_or_else(|_| dir.to_owned()))
}

/// The local review's file, in the git directory every worktree shares;
/// each comment's `branch` keeps branches apart. A linked worktree's file
/// from when it was in the worktree's own git directory moves there first,
/// once, when the shared one doesn't exist yet; when it does, the old one
/// is left where it is. `None` for paths compared outside git.
pub(crate) fn local_file(repo: &Repo) -> Result<Option<PathBuf>, String> {
    let Some(file) = shared_dir(repo).map(|dir| dir.join(LOCAL)) else {
        return Ok(None);
    };
    if let Some(old) = unmoved(repo, &file) {
        let _lock = lock(&file)?;
        if !file.exists() && old.exists() {
            fs::rename(&old, &file)
                .map_err(|e| format!("can't move {} to {}: {e}", old.display(), file.display()))?;
        }
    }
    Ok(Some(file))
}

/// [`local_file`] for readers that never write: the worktree's old file
/// while it hasn't moved.
pub(crate) fn local_file_read_only(repo: &Repo) -> Option<PathBuf> {
    let file = shared_dir(repo)?.join(LOCAL);
    Some(unmoved(repo, &file).unwrap_or(file))
}

/// The worktree's own old file, when it's to move to `file`.
fn unmoved(repo: &Repo, file: &Path) -> Option<PathBuf> {
    let old = repo.git_dir()?.canonicalize().ok()?.join(LOCAL);
    (old != file && old.exists() && !file.exists()).then_some(old)
}

/// The branch comments written now belong to: HEAD's, or while a rebase
/// stops, the branch being rebased. `None` on a detached HEAD, whatever
/// commit or range is shown.
pub(crate) fn current_branch(repo: &Repo) -> Option<String> {
    match repo.head().ok()? {
        Head::Branch(name) | Head::Unborn(name) => Some(name),
        Head::Detached(_) => {
            let git_dir = repo.git_dir()?;
            ["rebase-merge", "rebase-apply"].iter().find_map(|state| {
                let name = fs::read_to_string(git_dir.join(state).join("head-name")).ok()?;
                // A rebase of a detached HEAD writes `detached HEAD`.
                name.trim_end()
                    .strip_prefix("refs/heads/")
                    .map(str::to_owned)
            })
        }
    }
}

/// The review session the viewer has open, as the session pointer names
/// it: where `dffy comments --session` and agents find its comments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pointer {
    /// The session's comments file, absolute.
    pub comments: PathBuf,
    /// The review plugin's id for the review.
    pub review: String,
    /// The branch it was opened on; `None` on a detached HEAD.
    pub branch: Option<String>,
    /// The review's head commit.
    pub head: String,
}

/// Replaces the session pointer, in the git directory every worktree
/// shares. A comments path that isn't UTF-8 can't be written.
pub(crate) fn write_session(repo: &Repo, pointer: &Pointer) -> Result<(), String> {
    let path = shared_dir(repo)
        .ok_or("paths compared outside git have no review session")?
        .join(SESSION);
    let comments = pointer
        .comments
        .to_str()
        .ok_or_else(|| format!("{} isn't UTF-8", pointer.comments.display()))?;
    let mut out = format!("{{\"version\":{VERSION},\"comments\":");
    json::string(&mut out, comments);
    out.push_str(",\"review\":");
    json::string(&mut out, &pointer.review);
    out.push_str(",\"branch\":");
    match &pointer.branch {
        Some(branch) => json::string(&mut out, branch),
        None => out.push_str("null"),
    }
    out.push_str(",\"head\":");
    json::string(&mut out, &pointer.head);
    out.push_str("}\n");
    let _lock = lock(&path)?;
    replace(&path, out.as_bytes())
}

/// The session pointer, `None` when no session was opened.
pub(crate) fn read_session(repo: &Repo) -> Result<Option<Pointer>, String> {
    let Some(path) = shared_dir(repo).map(|dir| dir.join(SESSION)) else {
        return Ok(None);
    };
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("can't read {}: {e}", path.display())),
    };
    let bad = |what: &str| format!("{}: {what}", path.display());
    let value = json::parse(&bytes).map_err(|e| bad(&e))?;
    if value.get("version").and_then(Value::as_usize) != Some(VERSION) {
        return Err(bad(&format!("not a version {VERSION} session pointer")));
    }
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| bad(&format!("`{key}` must be a string")))
    };
    let comments = PathBuf::from(text("comments")?);
    if !comments.is_absolute() {
        return Err(bad("`comments` must be an absolute path"));
    }
    Ok(Some(Pointer {
        comments,
        review: text("review")?,
        branch: match value.get("branch") {
            None | Some(Value::Null) => None,
            Some(_) => Some(text("branch")?),
        },
        head: text("head")?,
    }))
}

/// Reads the file without locking: dffy replaces it whole by a rename, so a
/// reader sees one version or the next. A missing file is no comments.
pub(crate) fn read(path: &Path) -> Result<Review, String> {
    match fs::read(path) {
        Ok(bytes) => Review::parse(&bytes).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Review::default()),
        Err(e) => Err(format!("can't read {}: {e}", path.display())),
    }
}

/// Applies `ops` to the file under its lock, all or none, and returns the
/// id each one touched (`None` for a notice). Waits while another writer
/// holds the lock. Creates the file and its directory on first use.
pub(crate) fn apply(path: &Path, ops: Vec<Op>) -> Result<Vec<Option<String>>, String> {
    update(path, |comments| comments.apply(ops, SystemTime::now()))
}

/// Moves the threads whose first comments are `roots` from the file at
/// `from` to the one at `to`, each comment added there as [`Op::Add`]
/// adds one, with a new id, its replies after it; roots no longer in
/// `from` are skipped. `from` loses them only once `to` holds them: when
/// anything before writing `to` fails, neither file changes, and when
/// writing `from` after fails, they're in both. Locks `from`, then `to`.
/// They lose their `branch`: they're the review's now. Returns how many
/// comments moved.
pub(crate) fn move_threads(from: &Path, to: &Path, roots: &[String]) -> Result<usize, String> {
    update(from, |source| {
        let now = SystemTime::now();
        let moving = |c: &Comment| roots.contains(c.in_reply_to.as_ref().unwrap_or(&c.id));
        let moved: Vec<Comment> = source
            .comments
            .iter()
            .filter(|c| moving(c))
            .map(|c| Comment {
                branch: None,
                ..c.clone()
            })
            .collect();
        if moved.is_empty() {
            return Ok(0);
        }
        update(to, |target| {
            for root in moved.iter().filter(|c| c.in_reply_to.is_none()) {
                let reply_to = Target::Id(target.insert(root.clone(), None, now)?);
                for reply in moved
                    .iter()
                    .filter(|c| c.in_reply_to.as_ref() == Some(&root.id))
                {
                    target.insert(reply.clone(), Some(&reply_to), now)?;
                }
            }
            Ok(())
        })?;
        source.comments.retain(|c| !moving(c));
        Ok(moved.len())
    })
}

/// Reads the file under its lock, lets `change` change it, and writes it
/// back unless `change` fails. Waits while another writer holds the lock.
/// Creates the file and its directory on first use.
fn update<T>(
    path: &Path,
    change: impl FnOnce(&mut Review) -> Result<T, String>,
) -> Result<T, String> {
    let lock = lock(path)?;
    let mut comments = read(path)?;
    let out = change(&mut comments)?;
    replace(path, comments.encode().as_bytes())?;
    drop(lock);
    Ok(out)
}

/// Removes the threads `select` keeps from the file, under its lock, and
/// returns how many comments went.
pub(crate) fn remove_threads(path: &Path, select: &Select) -> Result<usize, String> {
    update(path, |review| {
        let gone: HashSet<String> = review
            .selected(select)
            .iter()
            .map(|c| c.id.clone())
            .collect();
        review.comments.retain(|c| !gone.contains(&c.id));
        Ok(gone.len())
    })
}

fn failed(what: &str, at: &Path, e: &io::Error) -> String {
    format!("can't {what} {}: {e}", at.display())
}

/// Takes `path`'s lock, waiting while another writer holds it, and creates
/// `path`'s directory on first use.
fn lock(path: &Path) -> Result<File, String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| failed("create", dir, &e))?;
    }
    // Never removed: deleting a lock file lets a waiting writer lock the
    // old one while a new writer locks a new one.
    let lock_path = lock_path(path);
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| failed("open", &lock_path, &e))?;
    lock.lock().map_err(|e| failed("lock", &lock_path, &e))?;
    Ok(lock)
}

/// Writes `bytes` over `path` by a rename, so readers see one version or
/// the next; the caller holds `path`'s lock.
fn replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    // One fixed name is enough: only the lock's holder writes it. One left
    // by a crash is removed, and `create_new` won't follow a link put in
    // its place.
    let tmp = sibling(path, ".tmp");
    fs::remove_file(&tmp).ok();
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| failed("create", &tmp, &e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| failed("write", &tmp, &e))?;
    fs::rename(&tmp, path).map_err(|e| failed("replace", path, &e))
}

/// `dffy comments apply`: the operations' JSON in, the answer's out.
/// `branch`, for the local review's file, goes to each thread `add` starts
/// without one.
pub(crate) fn apply_json(
    path: &Path,
    input: &[u8],
    branch: Option<&str>,
) -> Result<String, String> {
    let ids = apply(path, parse_ops(input, branch)?)?;
    let mut out = String::from("{\"ids\":[");
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match id {
            Some(id) => json::string(&mut out, id),
            None => out.push_str("null"),
        }
    }
    out.push_str("]}\n");
    Ok(out)
}

fn lock_path(path: &Path) -> PathBuf {
    sibling(path, ".lock")
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

impl Review {
    /// Parses a file. Blank bytes are no comments; any `version` but 1 is
    /// refused, so a newer file is never rewritten in this one's format.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Self::default());
        }
        let Value::Object(fields) = json::parse(bytes)? else {
            return Err("not a JSON object".to_owned());
        };
        match fields
            .iter()
            .find(|(k, _)| k == "version")
            .map(|(_, v)| v.as_usize())
        {
            Some(Some(VERSION)) => {}
            Some(Some(v)) => {
                return Err(format!(
                    "comments file version {v}; this dffy reads version {VERSION}"
                ));
            }
            _ => return Err("not a comments file: no version".to_owned()),
        }
        let mut out = Self::default();
        for (key, value) in fields {
            match key.as_str() {
                "version" => {}
                "notice" => out.notice = string(&key, value)?,
                "summary" => out.summary = string(&key, value)?,
                "comments" => {
                    let Value::Array(items) = value else {
                        return Err("`comments` must be a list".to_owned());
                    };
                    for (i, item) in items.into_iter().enumerate() {
                        let comment = match item {
                            Value::Object(fields) => Comment::from_fields(fields),
                            _ => Err("not an object".to_owned()),
                        }
                        .and_then(|c| {
                            if c.id.is_empty() {
                                Err("no id".to_owned())
                            } else {
                                Ok(c)
                            }
                        })
                        .map_err(|e| format!("comment {i}: {e}"))?;
                        out.comments.push(comment);
                    }
                }
                _ => out.extra.push((key, value)),
            }
        }
        Ok(out)
    }

    /// The file's bytes, a comment per line.
    pub(crate) fn encode(&self) -> String {
        let mut out = format!("{{\"version\":{VERSION}");
        if let Some(notice) = &self.notice {
            out.push_str(",\"notice\":");
            json::string(&mut out, notice);
        }
        if let Some(summary) = &self.summary {
            out.push_str(",\"summary\":");
            json::string(&mut out, summary);
        }
        for (key, value) in &self.extra {
            out.push(',');
            json::string(&mut out, key);
            out.push(':');
            json::write(&mut out, value);
        }
        out.push_str(",\"comments\":[");
        for (i, comment) in self.comments.iter().enumerate() {
            out.push_str(if i == 0 { "\n" } else { ",\n" });
            json::write(&mut out, &Value::Object(comment.to_fields()));
        }
        out.push_str("\n]}\n");
        out
    }

    /// Applies `ops` in order, all or none: on an error `self` is left as
    /// it was, and the message names the failing operation's index. `now`
    /// dates added comments.
    pub(crate) fn apply(
        &mut self,
        ops: Vec<Op>,
        now: SystemTime,
    ) -> Result<Vec<Option<String>>, String> {
        let mut next = self.clone();
        let mut ids = Vec::with_capacity(ops.len());
        for (i, op) in ops.into_iter().enumerate() {
            let name = op.name();
            let id = next
                .apply_one(op, now)
                .map_err(|e| format!("operation {i} ({name}): {e}"))?;
            ids.push(id);
        }
        *self = next;
        Ok(ids)
    }

    fn apply_one(&mut self, op: Op, now: SystemTime) -> Result<Option<String>, String> {
        match op {
            Op::Add { comment, reply_to } => {
                self.insert(*comment, reply_to.as_ref(), now).map(Some)
            }
            Op::Set { target, change } => self.set(&target, change).map(Some),
            Op::Upsert { fields, reply_to } => {
                if fields.iter().any(|(k, _)| k == "id") {
                    return Err("dffy assigns the id".to_owned());
                }
                let Some(reference) = fields
                    .iter()
                    .find(|(k, _)| k == "ref")
                    .and_then(|(_, v)| v.as_str())
                else {
                    return Err("an upsert needs a ref".to_owned());
                };
                let Ok(i) = self.find(&Target::Ref(reference.to_owned())) else {
                    let comment = Comment::from_new_fields(fields)?;
                    return self.insert(comment, reply_to.as_ref(), now).map(Some);
                };
                let mut merged = self.comments[i].to_fields();
                for (key, value) in fields {
                    let at = merged.iter().position(|(k, _)| *k == key);
                    match (at, value) {
                        (Some(at), Value::Null) => {
                            merged.remove(at);
                        }
                        (Some(at), value) => merged[at].1 = value,
                        (None, Value::Null) => {}
                        (None, value) => merged.push((key, value)),
                    }
                }
                let mut comment = Comment::from_new_fields(merged)?;
                if let Some(target) = &reply_to {
                    self.join_thread(&mut comment, target)?;
                    let id = &comment.id;
                    if comment.in_reply_to.as_ref() == Some(id) {
                        return Err(format!("comment {id} can't reply in its own thread"));
                    }
                    // Replies hang off the first comment alone.
                    if self
                        .comments
                        .iter()
                        .any(|c| c.in_reply_to.as_ref() == Some(id))
                    {
                        return Err(format!(
                            "comment {id} has replies, so it can't become a reply itself"
                        ));
                    }
                }
                self.check(&comment, Some(i))?;
                self.comments[i] = comment;
                Ok(Some(self.comments[i].id.clone()))
            }
            Op::Rm(target) => {
                let id = self.comments[self.find(&target)?].id.clone();
                self.comments
                    .retain(|c| c.id != id && c.in_reply_to.as_ref() != Some(&id));
                Ok(Some(id))
            }
            Op::Notice(notice) => {
                self.notice = notice;
                Ok(None)
            }
            Op::Summary(summary) => {
                self.summary = summary;
                Ok(None)
            }
        }
    }

    /// [`Op::Set`]: returns the id of the comment changed.
    fn set(&mut self, target: &Target, change: Change) -> Result<String, String> {
        let i = self.find(target)?;
        if change.anchor.as_ref().is_some_and(|a| a.lines.is_empty()) {
            return Err("an anchor needs at least one line".to_owned());
        }
        if change.author.as_ref().is_some_and(|a| !a.is_empty()) {
            return Err(
                "`author` can only be set to \"\", which makes the comment yours".to_owned(),
            );
        }
        if let Some(branch) = change.branch {
            let named = &self.comments[i];
            let root = named.in_reply_to.as_ref().unwrap_or(&named.id).clone();
            for c in &mut self.comments {
                if c.id == root || c.in_reply_to.as_ref() == Some(&root) {
                    c.branch.clone_from(&branch);
                }
            }
        }
        let comment = &mut self.comments[i];
        let posted = comment.reference.is_some() && change.reference != Some(None);
        for (field, given) in [
            ("text", change.text.is_some()),
            ("suggestion", change.suggestion.is_some()),
            ("author", change.author.is_some()),
        ] {
            if given && posted {
                return Err(format!(
                    "comment {} is posted (it has a ref), so its {field} is the host's",
                    comment.id
                ));
            }
        }
        if let Some(text) = change.text {
            comment.text = text;
        }
        if let Some(suggestion) = change.suggestion {
            comment.suggestion = suggestion;
        }
        if let Some(author) = change.author {
            comment.author = author;
        }
        if let Some(resolved) = change.resolved {
            comment.resolved = resolved;
        }
        if let Some(reference) = change.reference {
            comment.reference = reference;
        }
        if let Some(anchor) = change.anchor {
            comment.anchor = Some(anchor);
        }
        if let Some(pending) = change.pending {
            comment.pending = pending;
        }
        if let Some(post_error) = change.post_error {
            comment.post_error = post_error;
        }
        self.check(&self.comments[i], Some(i))?;
        Ok(self.comments[i].id.clone())
    }

    fn insert(
        &mut self,
        mut comment: Comment,
        reply_to: Option<&Target>,
        now: SystemTime,
    ) -> Result<String, String> {
        comment.in_reply_to = None;
        if let Some(target) = reply_to {
            self.join_thread(&mut comment, target)?;
        }
        self.check(&comment, None)?;
        if comment.created.is_empty() {
            comment.created = timestamp(now);
        }
        comment.id = loop {
            // Random, so an id is never handed out again after a removal.
            let id = format!("{:016x}", RandomState::new().hash_one(()));
            if !self.comments.iter().any(|c| c.id == id) {
                break id;
            }
        };
        self.comments.push(comment);
        Ok(self.comments[self.comments.len() - 1].id.clone())
    }

    /// Makes `comment` a reply in `target`'s thread: it takes the thread's
    /// place and branch and has none of its own.
    fn join_thread(&self, comment: &mut Comment, target: &Target) -> Result<(), String> {
        let parent = &self.comments[self.find(target)?];
        let root_id = parent.in_reply_to.as_ref().unwrap_or(&parent.id);
        let root = self
            .comments
            .iter()
            .find(|c| c.id == *root_id)
            .unwrap_or(parent);
        *comment = Comment {
            path: root.path.clone(),
            old_path: root.old_path.clone(),
            branch: root.branch.clone(),
            in_reply_to: Some(root_id.clone()),
            side: Side::default(),
            start_side: None,
            start: None,
            end: None,
            anchor: None,
            distance: None,
            ..std::mem::take(comment)
        };
        Ok(())
    }

    /// What every comment written needs; `at` is the comment's own index
    /// when it replaces one.
    fn check(&self, comment: &Comment, at: Option<usize>) -> Result<(), String> {
        if let Some(reference) = &comment.reference {
            self.check_ref(reference, at)?;
        }
        if comment.anchor.is_some() && (comment.in_reply_to.is_some() || comment.end.is_none()) {
            return Err("only a line comment has an anchor".to_owned());
        }
        if comment.in_reply_to.is_some() {
            return Ok(());
        }
        // A general comment comes only from an integration, which names it
        // by its ref.
        if comment.path.is_empty() {
            if comment.reference.is_none() {
                return Err("a comment needs a path".to_owned());
            }
            if comment.start.is_some() || comment.end.is_some() {
                return Err("a general comment, without a path, has no lines".to_owned());
            }
            return Ok(());
        }
        let (Some(start), Some(end)) = (comment.start, comment.end) else {
            if comment.start.is_some() || comment.end.is_some() {
                return Err("a line comment needs both start and end".to_owned());
            }
            return Ok(());
        };
        let start_side = comment.start_side.unwrap_or(comment.side);
        if start.on(start_side).is_none() {
            return Err(format!("start has no {} line", start_side.name()));
        }
        if end.on(comment.side).is_none() {
            return Err(format!("end has no {} line", comment.side.name()));
        }
        if comment.anchor.as_ref().is_none_or(|a| a.lines.is_empty()) {
            return Err("a line comment needs an anchor with its lines".to_owned());
        }
        Ok(())
    }

    fn check_ref(&self, reference: &str, at: Option<usize>) -> Result<(), String> {
        match self
            .comments
            .iter()
            .enumerate()
            .find(|&(i, c)| Some(i) != at && c.reference.as_deref() == Some(reference))
        {
            Some((_, other)) => Err(format!(
                "ref {reference} is already on comment {}",
                other.id
            )),
            None => Ok(()),
        }
    }

    fn find(&self, target: &Target) -> Result<usize, String> {
        match target {
            Target::Id(id) => self
                .comments
                .iter()
                .position(|c| c.id == *id)
                .ok_or_else(|| format!("comment {id} doesn't exist")),
            Target::Ref(reference) => self
                .comments
                .iter()
                .position(|c| c.reference.as_ref() == Some(reference))
                .ok_or_else(|| format!("no comment has ref {reference}")),
        }
    }

    /// The comments of the threads `select` keeps, in file order.
    pub(crate) fn selected(&self, select: &Select) -> Vec<&Comment> {
        let by_id: HashMap<&str, &Comment> =
            self.comments.iter().map(|c| (c.id.as_str(), c)).collect();
        self.comments
            .iter()
            .filter(|c| {
                let root = c
                    .in_reply_to
                    .as_deref()
                    .and_then(|id| by_id.get(id).copied())
                    .unwrap_or(c);
                select.keeps(root)
            })
            .collect()
    }
}

/// Which threads a listing or a removal takes, each judged by its first
/// comment: a reply is open or resolved, on a file and on a branch as its
/// thread is. The default takes every thread.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Select<'a> {
    /// `Some(true)` takes resolved threads only, `Some(false)` open ones.
    pub resolved: Option<bool>,
    /// The threads on this file, by its path or its old one; general
    /// comments are on none.
    pub path: Option<&'a str>,
    pub branches: Branches<'a>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) enum Branches<'a> {
    #[default]
    All,
    /// The threads on this branch and those on none; `On(None)`, for a
    /// detached HEAD, only those on none.
    On(Option<&'a str>),
}

impl Select<'_> {
    fn keeps(&self, root: &Comment) -> bool {
        self.resolved.is_none_or(|r| root.resolved == r)
            && self
                .path
                .is_none_or(|p| root.path == p || root.old_path.as_deref() == Some(p))
            && match self.branches {
                Branches::All => true,
                Branches::On(branch) => root.on_branch(branch),
            }
    }
}

impl Comment {
    /// The lines it's on, 0-based on `side`. A range that starts on the
    /// other side covers `side`'s lines from `start`'s number there, when it
    /// has one, else just `end`. `None` for a file comment or a reply.
    pub(crate) fn range(&self) -> Option<(Side, Range<usize>)> {
        let end = self.end?.on(self.side)?;
        let start = self
            .start
            .and_then(|s| s.on(self.side))
            .map_or(end, |s| s.min(end));
        Some((self.side, start..end + 1))
    }

    /// Whether it shows on `branch`, `None` for a detached HEAD: a comment
    /// on no branch shows on every one. For a thread, ask its first comment.
    pub(crate) fn on_branch(&self, branch: Option<&str>) -> bool {
        self.branch.is_none() || self.branch.as_deref() == branch
    }

    /// A comment from an operation, whose text must be given.
    fn from_new_fields(fields: Fields) -> Result<Self, String> {
        if !fields
            .iter()
            .any(|(k, v)| k == "text" && !matches!(v, Value::Null))
        {
            return Err("a comment needs text".to_owned());
        }
        Self::from_fields(fields)
    }

    /// Fields of the wrong type are errors; unknown ones are kept.
    fn from_fields(fields: Fields) -> Result<Self, String> {
        let mut c = Self::default();
        for (key, value) in fields {
            match key.as_str() {
                "id" => c.id = string(&key, value)?.unwrap_or_default(),
                "path" => c.path = string(&key, value)?.unwrap_or_default(),
                "old_path" => c.old_path = string(&key, value)?,
                "side" => c.side = side(&key, value)?.unwrap_or_default(),
                "start_side" => c.start_side = side(&key, value)?,
                "start" => c.start = line_numbers(&key, &value)?,
                "end" => c.end = line_numbers(&key, &value)?,
                "text" => c.text = string(&key, value)?.unwrap_or_default(),
                "suggestion" => c.suggestion = string(&key, value)?,
                "author" => c.author = string(&key, value)?.unwrap_or_default(),
                "created" => c.created = string(&key, value)?.unwrap_or_default(),
                "in_reply_to" => c.in_reply_to = string(&key, value)?,
                "resolved" => {
                    c.resolved = match value {
                        Value::Null => false,
                        Value::Bool(b) => b,
                        _ => return Err("`resolved` must be true or false".to_owned()),
                    };
                }
                "ref" => c.reference = string(&key, value)?,
                "anchor" => c.anchor = anchor(value)?,
                "url" => c.url = string(&key, value)?,
                "kind" => c.kind = string(&key, value)?,
                "label" => c.label = string(&key, value)?,
                "branch" => c.branch = string(&key, value)?,
                "post_error" => c.post_error = string(&key, value)?,
                "outdated" | "pending" => {
                    let flag = match value {
                        Value::Null => false,
                        Value::Bool(b) => b,
                        _ => return Err(format!("`{key}` must be true or false")),
                    };
                    if key == "outdated" {
                        c.outdated = flag;
                    } else {
                        c.pending = flag;
                    }
                }
                "distance" => {
                    c.distance = match value {
                        Value::Null => None,
                        _ => Some(
                            value
                                .as_usize()
                                .ok_or("`distance` must be a whole number")?,
                        ),
                    };
                }
                _ => c.extra.push((key, value)),
            }
        }
        Ok(c)
    }

    /// The file's fields in order, leaving out those that don't apply.
    pub(crate) fn to_fields(&self) -> Fields {
        let text = |s: &str| Value::String(s.to_owned());
        let mut f = vec![("id".to_owned(), text(&self.id))];
        let put = |f: &mut Fields, key: &str, value: Option<Value>| {
            if let Some(value) = value {
                f.push((key.to_owned(), value));
            }
        };
        put(
            &mut f,
            "path",
            (!self.path.is_empty()).then(|| text(&self.path)),
        );
        put(&mut f, "old_path", self.old_path.as_deref().map(text));
        if self.end.is_some() {
            put(&mut f, "side", Some(text(self.side.name())));
        }
        put(
            &mut f,
            "start_side",
            self.start_side.map(|s| text(s.name())),
        );
        put(&mut f, "start", self.start.map(LineNumbers::to_value));
        put(&mut f, "end", self.end.map(LineNumbers::to_value));
        put(&mut f, "text", Some(text(&self.text)));
        put(&mut f, "suggestion", self.suggestion.as_deref().map(text));
        put(&mut f, "author", Some(text(&self.author)));
        put(&mut f, "created", Some(text(&self.created)));
        put(&mut f, "in_reply_to", self.in_reply_to.as_deref().map(text));
        put(&mut f, "resolved", Some(Value::Bool(self.resolved)));
        put(&mut f, "ref", self.reference.as_deref().map(text));
        put(
            &mut f,
            "anchor",
            self.anchor.as_ref().map(|a| {
                let list = |lines: &[String]| Value::Array(lines.iter().map(|l| text(l)).collect());
                Value::Object(vec![
                    ("before".to_owned(), list(&a.before)),
                    ("lines".to_owned(), list(&a.lines)),
                    ("after".to_owned(), list(&a.after)),
                ])
            }),
        );
        put(&mut f, "distance", self.distance.map(number));
        put(&mut f, "url", self.url.as_deref().map(text));
        put(
            &mut f,
            "outdated",
            self.outdated.then_some(Value::Bool(true)),
        );
        put(&mut f, "kind", self.kind.as_deref().map(text));
        put(&mut f, "label", self.label.as_deref().map(text));
        put(&mut f, "branch", self.branch.as_deref().map(text));
        put(&mut f, "pending", self.pending.then_some(Value::Bool(true)));
        put(&mut f, "post_error", self.post_error.as_deref().map(text));
        f.extend(self.extra.iter().cloned());
        f
    }
}

impl LineNumbers {
    fn to_value(self) -> Value {
        let n = |n: Option<usize>| n.map_or(Value::Null, |n| number(n + 1));
        Value::Object(vec![
            ("old".to_owned(), n(self.old)),
            ("new".to_owned(), n(self.new)),
        ])
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "line numbers and distances are far below 2^53, where f64 is exact"
)]
fn number(n: usize) -> Value {
    Value::Number(n as f64)
}

/// A string field; `null` is left out.
fn string(key: &str, value: Value) -> Result<Option<String>, String> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s)),
        _ => Err(format!("`{key}` must be a string")),
    }
}

fn side(key: &str, value: Value) -> Result<Option<Side>, String> {
    match string(key, value)?.as_deref() {
        None => Ok(None),
        Some("old") => Ok(Some(Side::Old)),
        Some("new") => Ok(Some(Side::New)),
        Some(_) => Err(format!("`{key}` must be old or new")),
    }
}

fn line_numbers(key: &str, value: &Value) -> Result<Option<LineNumbers>, String> {
    let bad = || {
        format!(
            "`{key}` must be {{\"old\": <line or null>, \"new\": <line or null>}}, lines from 1"
        )
    };
    let number = |side: &str| match value.get(side) {
        None | Some(Value::Null) => Ok(None),
        Some(n) => n
            .as_usize()
            .and_then(|n| n.checked_sub(1))
            .map(Some)
            .ok_or_else(bad),
    };
    match value {
        Value::Null => Ok(None),
        Value::Object(_) => match (number("old")?, number("new")?) {
            (None, None) => Err(bad()),
            (old, new) => Ok(Some(LineNumbers { old, new })),
        },
        _ => Err(bad()),
    }
}

fn anchor(value: Value) -> Result<Option<Anchor>, String> {
    let fields = match value {
        Value::Null => return Ok(None),
        Value::Object(fields) => fields,
        _ => return Err("`anchor` must be an object".to_owned()),
    };
    let mut anchor = Anchor::default();
    for (key, value) in fields {
        let list = match key.as_str() {
            "before" => &mut anchor.before,
            "lines" => &mut anchor.lines,
            "after" => &mut anchor.after,
            _ => continue,
        };
        let bad = || format!("`anchor.{key}` must be a list of strings");
        let Value::Array(items) = value else {
            return Err(bad());
        };
        *list = items
            .into_iter()
            .map(|item| match item {
                Value::String(s) => Ok(s),
                _ => Err(bad()),
            })
            .collect::<Result<_, _>>()?;
    }
    Ok(Some(anchor))
}

/// Parses `dffy comments apply`'s input, a JSON list of operations. A
/// thread `add` starts without a `branch` field gets `branch`; one given
/// as `null` stays on no branch.
pub(crate) fn parse_ops(input: &[u8], branch: Option<&str>) -> Result<Vec<Op>, String> {
    let Value::Array(items) = json::parse(input)? else {
        return Err("the operations must be a JSON list".to_owned());
    };
    items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let name = item
                .get("op")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_owned();
            parse_op(item, branch).map_err(|e| format!("operation {i} ({name}): {e}"))
        })
        .collect()
}

fn parse_op(item: Value, branch: Option<&str>) -> Result<Op, String> {
    let Value::Object(mut fields) = item else {
        return Err("not an object".to_owned());
    };
    match take(&mut fields, "op").as_ref().and_then(Value::as_str) {
        Some("add") => {
            let (mut fields, reply_to) = comment_fields(take(&mut fields, "comment"))?;
            if fields.iter().any(|(k, _)| k == "id") {
                return Err("dffy assigns the id".to_owned());
            }
            if let Some(branch) = branch.filter(|_| reply_to.is_none())
                && !fields.iter().any(|(k, _)| k == "branch")
            {
                fields.push(("branch".to_owned(), Value::String(branch.to_owned())));
            }
            Ok(Op::Add {
                comment: Box::new(Comment::from_new_fields(fields)?),
                reply_to,
            })
        }
        Some("set") => Ok(Op::Set {
            target: target(take(&mut fields, "id"), take(&mut fields, "ref"))?,
            change: change(take(&mut fields, "fields"))?,
        }),
        Some("upsert") => {
            let (fields, reply_to) = comment_fields(take(&mut fields, "comment"))?;
            Ok(Op::Upsert { fields, reply_to })
        }
        Some("rm") => Ok(Op::Rm(target(
            take(&mut fields, "id"),
            take(&mut fields, "ref"),
        )?)),
        Some("notice") => match take(&mut fields, "notice") {
            Some(value) => Ok(Op::Notice(string("notice", value)?)),
            None => Err("needs a notice, a string or null".to_owned()),
        },
        Some("summary") => match take(&mut fields, "summary") {
            Some(value) => Ok(Op::Summary(string("summary", value)?)),
            None => Err("needs a summary, a string or null".to_owned()),
        },
        Some(other) => Err(format!("unknown op {other}")),
        None => Err("needs an op".to_owned()),
    }
}

fn take(fields: &mut Fields, key: &str) -> Option<Value> {
    let at = fields.iter().position(|(k, _)| k == key)?;
    Some(fields.remove(at).1)
}

/// An operation's comment, and the thread it replies to.
fn comment_fields(value: Option<Value>) -> Result<(Fields, Option<Target>), String> {
    let Some(Value::Object(mut fields)) = value else {
        return Err("needs a comment object".to_owned());
    };
    let reply_to = match (
        take(&mut fields, "in_reply_to"),
        take(&mut fields, "in_reply_to_ref"),
    ) {
        (Some(Value::String(id)), None | Some(Value::Null)) => Some(Target::Id(id)),
        (None | Some(Value::Null), Some(Value::String(reference))) => Some(Target::Ref(reference)),
        (None | Some(Value::Null), None | Some(Value::Null)) => None,
        _ => {
            return Err("a reply needs in_reply_to or in_reply_to_ref, a string".to_owned());
        }
    };
    Ok((fields, reply_to))
}

fn target(id: Option<Value>, reference: Option<Value>) -> Result<Target, String> {
    match (id, reference) {
        (Some(Value::String(id)), None) => Ok(Target::Id(id)),
        (None, Some(Value::String(reference))) => Ok(Target::Ref(reference)),
        _ => Err("needs an id or a ref, a string".to_owned()),
    }
}

fn change(value: Option<Value>) -> Result<Change, String> {
    let Some(Value::Object(fields)) = value else {
        return Err("needs a fields object".to_owned());
    };
    let mut change = Change::default();
    let flag = |key: &str, value| match value {
        Value::Bool(b) => Ok(Some(b)),
        _ => Err(format!("`{key}` must be true or false")),
    };
    for (key, value) in fields {
        match key.as_str() {
            "text" => change.text = Some(string(&key, value)?.ok_or("`text` can't be null")?),
            "resolved" => change.resolved = flag(&key, value)?,
            "ref" => change.reference = Some(string(&key, value)?),
            "anchor" => change.anchor = Some(anchor(value)?.ok_or("`anchor` can't be null")?),
            "author" => change.author = Some(string(&key, value)?.ok_or("`author` can't be null")?),
            "suggestion" => change.suggestion = Some(string(&key, value)?),
            "branch" => change.branch = Some(string(&key, value)?),
            "pending" => change.pending = flag(&key, value)?,
            "post_error" => change.post_error = Some(string(&key, value)?),
            _ => {
                return Err(format!(
                    "`{key}` can't be set; set changes text, resolved, ref, anchor, author, \
                     suggestion, branch, pending and post_error"
                ));
            }
        }
    }
    Ok(change)
}

impl Anchor {
    /// The lines `range` of `text`, a non-empty range within it, with their
    /// context.
    pub(crate) fn new(text: &Text, range: Range<usize>) -> Self {
        let line = |i| String::from_utf8_lossy(without_newline(text.line(i))).into_owned();
        Self {
            before: (range.start.saturating_sub(CONTEXT)..range.start)
                .map(line)
                .collect(),
            after: (range.end..(range.end + CONTEXT).min(text.len()))
                .map(line)
                .collect(),
            lines: range.map(line).collect(),
        }
    }

    /// Where the anchored lines are in `text`, recorded as starting at
    /// line `at`: there when they still match, else wherever the lines and
    /// their context appear exactly once within [`NEARBY`] lines of it.
    /// `None` leaves the comment unplaced.
    pub(crate) fn place(&self, text: &Text, at: usize) -> Option<Range<usize>> {
        let len = self.lines.len();
        if matches_at(text, at, &self.lines) {
            return Some(at..at + len);
        }
        let mut found = None;
        for first in at.saturating_sub(NEARBY)..=at.saturating_add(NEARBY).min(text.len()) {
            let Some(before) = first.checked_sub(self.before.len()) else {
                continue;
            };
            if matches_at(text, before, &self.before)
                && matches_at(text, first, &self.lines)
                && matches_at(text, first + len, &self.after)
            {
                if found.is_some() {
                    return None;
                }
                found = Some(first..first + len);
            }
        }
        found
    }
}

fn matches_at(text: &Text, at: usize, lines: &[String]) -> bool {
    at + lines.len() <= text.len()
        && lines.iter().enumerate().all(|(k, want)| {
            let line = without_newline(text.line(at + k));
            line == want.as_bytes()
                || (std::str::from_utf8(line).is_err() && String::from_utf8_lossy(line) == **want)
        })
}

fn without_newline(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n").unwrap_or(line)
}

/// Lines from `range` (non-empty, 0-based on `side`) to the nearest change
/// in `hunks`, 0 when it includes one. A change on the other side sits
/// between two of this side's lines, each 1 from it. `None` without
/// changes.
pub(crate) fn distance(range: Range<usize>, side: Side, hunks: &[Hunk]) -> Option<usize> {
    let last = range.end - 1;
    hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter_map(|line| {
            let (on_side, at) = match (line.kind, side) {
                (LineKind::Context, _) => return None,
                (LineKind::Added, Side::New) => (true, line.new),
                (LineKind::Removed, Side::Old) => (true, line.old),
                (LineKind::Added, Side::Old) => (false, line.old),
                (LineKind::Removed, Side::New) => (false, line.new),
            };
            Some(if on_side {
                if at < range.start {
                    range.start - at
                } else {
                    at.saturating_sub(last)
                }
            } else if at <= range.start {
                range.start - at + 1
            } else {
                at.saturating_sub(last)
            })
        })
        .min()
}

/// `comments` in thread order, for listing: each thread's first comment,
/// then its replies in the order they were added. General comments come
/// first, then threads by path and first line, file comments first. A
/// reply whose thread isn't among `comments` stands as a thread of its own.
pub(crate) fn threads<'a>(comments: impl IntoIterator<Item = &'a Comment>) -> Vec<&'a Comment> {
    let all: Vec<&Comment> = comments.into_iter().collect();
    let ids: HashSet<&str> = all.iter().map(|c| c.id.as_str()).collect();
    let thread = |c: &'a Comment| c.in_reply_to.as_deref().filter(|id| ids.contains(id));
    let mut replies: HashMap<&str, Vec<&Comment>> = HashMap::new();
    let mut roots = Vec::new();
    for &c in &all {
        match thread(c) {
            Some(root) => replies.entry(root).or_default().push(c),
            None => roots.push(c),
        }
    }
    roots.sort_by_key(|c| (&c.path, c.range().map(|(_, lines)| lines.start)));
    roots
        .into_iter()
        .flat_map(|root| {
            std::iter::once(root).chain(replies.remove(root.id.as_str()).unwrap_or_default())
        })
        .collect()
}

/// A comment's author as shown: `you` for an empty one, the rest escaped.
pub(crate) fn who(author: &str) -> String {
    if author.is_empty() {
        "you".to_owned()
    } else {
        escape(author.as_bytes())
    }
}

/// Writes `comments`, in [`threads`] order, for reading: a heading with
/// each thread's place, and its branch when it isn't `here`, then each
/// comment's id, author and time over its text. A reply not under its
/// thread's first comment, as on a page that starts mid-thread, is headed
/// by its thread's id.
pub(crate) fn render(comments: &[&Comment], here: Option<&str>, c: &mut Canvas) {
    let text = |c: &mut Canvas, indent: usize, text: &str| {
        for line in text.lines() {
            c.spaces(Role::Plain, indent);
            c.line(Role::Plain, &escape(line.as_bytes()));
        }
    };
    let mut thread = None;
    for (i, comment) in comments.iter().enumerate() {
        if comment.in_reply_to.is_none() || comment.in_reply_to.as_deref() != thread {
            thread = Some(comment.in_reply_to.as_deref().unwrap_or(&comment.id));
            if i > 0 {
                c.end_line();
            }
            if comment.path.is_empty() {
                c.push(Role::FileHeader, "conversation");
                for word in [&comment.kind, &comment.label].into_iter().flatten() {
                    c.push(Role::Dim, &format!(", {}", escape(word.as_bytes())));
                }
            } else {
                c.push(Role::FileHeader, &escape(comment.path.as_bytes()));
            }
            match (&comment.in_reply_to, comment.range()) {
                (Some(root), _) => c.push(
                    Role::Dim,
                    &format!(", reply in thread {}", escape(root.as_bytes())),
                ),
                (None, Some((side, lines))) => {
                    let (first, last) = (lines.start + 1, lines.end);
                    let mut place = if first == last {
                        format!(":{first}")
                    } else {
                        format!(":{first}-{last}")
                    };
                    if side == Side::Old {
                        place.push_str(" (old side)");
                    }
                    c.push(Role::FileHeader, &place);
                }
                (None, None) => {}
            }
            if let Some(branch) = comment.branch.as_deref().filter(|b| Some(*b) != here) {
                c.push(
                    Role::Dim,
                    &format!(", branch {}", escape(branch.as_bytes())),
                );
            }
            if comment.resolved {
                c.push(Role::Dim, ", resolved");
            }
            c.end_line();
        }
        c.spaces(Role::Plain, 2);
        c.push(Role::Dim, &escape(comment.id.as_bytes()));
        c.push(Role::Plain, " ");
        c.push(Role::Plain, &who(&comment.author));
        c.push(
            Role::Dim,
            &format!(", {}", escape(comment.created.as_bytes())),
        );
        if comment.reference.is_some() {
            c.push(Role::Dim, ", posted");
        }
        if comment.pending {
            c.push(Role::Dim, ", pending");
        }
        c.end_line();
        text(c, 4, &comment.text);
        if let Some(suggestion) = &comment.suggestion {
            c.spaces(Role::Plain, 4);
            c.line(Role::Dim, "suggestion:");
            text(c, 6, suggestion);
        }
        if let Some(error) = &comment.post_error {
            c.spaces(Role::Plain, 4);
            c.line(
                Role::Warn,
                &format!("not posted: {}", escape(error.as_bytes())),
            );
        }
    }
}

/// `comments`, in [`threads`] order, as Markdown: `summary` first when
/// there's one, then a heading per thread with its place, the commented
/// lines as they were in a code block, and each comment's author and time
/// over its text, which is Markdown already.
pub(crate) fn markdown(comments: &[&Comment], summary: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(summary) = summary {
        out.push_str("## Summary\n\n");
        out.push_str(summary.trim_end());
        out.push_str("\n\n");
    }
    let mut thread = None;
    for comment in comments {
        if comment.in_reply_to.is_none() || comment.in_reply_to.as_deref() != thread {
            thread = Some(comment.in_reply_to.as_deref().unwrap_or(&comment.id));
            out.push_str("## ");
            if comment.path.is_empty() {
                out.push_str("Conversation");
                for word in [&comment.kind, &comment.label].into_iter().flatten() {
                    out.push_str(", ");
                    out.push_str(word);
                }
            } else {
                let ticks = fence(&comment.path, 1);
                let pad = if comment.path.starts_with('`') || comment.path.ends_with('`') {
                    " "
                } else {
                    ""
                };
                let _ = write!(out, "{ticks}{pad}{}{pad}{ticks}", comment.path);
            }
            match (&comment.in_reply_to, comment.range()) {
                (Some(root), _) => {
                    let _ = write!(out, ", reply in thread {root}");
                }
                (None, Some((side, lines))) => {
                    let (first, last) = (lines.start + 1, lines.end);
                    let _ = if first == last {
                        write!(out, ", line {first}")
                    } else {
                        write!(out, ", lines {first}-{last}")
                    };
                    if side == Side::Old {
                        out.push_str(" (old side)");
                    }
                }
                (None, None) => {}
            }
            if comment.resolved {
                out.push_str(", resolved");
            }
            out.push_str("\n\n");
            if comment.in_reply_to.is_none()
                && let Some(anchor) = &comment.anchor
            {
                code_block(&mut out, &anchor.lines.join("\n"));
            }
        }
        let author = if comment.author.is_empty() {
            "you"
        } else {
            &comment.author
        };
        let _ = write!(out, "**{author}**, {}\n\n", comment.created);
        out.push_str(comment.text.trim_end());
        out.push_str("\n\n");
        if let Some(suggestion) = &comment.suggestion {
            out.push_str("Suggested change:\n\n");
            code_block(&mut out, suggestion);
        }
    }
    out.truncate(out.trim_end().len());
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// A fenced code block of `text`, its fence longer than any run of
/// backticks in it.
fn code_block(out: &mut String, text: &str) {
    let ticks = fence(text, 3);
    let _ = write!(out, "{ticks}\n{text}\n{ticks}\n\n");
}

/// Backticks, at least `min` and one more than the longest run in `text`.
fn fence(text: &str, min: usize) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat((longest + 1).max(min))
}

/// `t` in RFC 3339, UTC, to the second.
fn timestamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's civil_from_days, for days on or after 1970.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests;

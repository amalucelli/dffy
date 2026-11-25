//! Review comments: the comments file that the viewer draws and
//! integrations keep in step with a code host (docs/comments.md is the
//! contract). This module owns the format and the operations that write the
//! file under a lock; a comment's anchor is the core's
//! [`dffy_core::annotation::Anchor`], which places it. Its submodules are the review layer frontends share: the threads made
//! from the file ([`model`]), the comments panel's list ([`listing`]), a
//! thread or a selection as a prompt for a coding agent ([`prompt`]), a
//! review session with a plugin ([`session`]) and asking an agent about a
//! thread ([`ask`]). Nothing here knows about the viewer.

pub(crate) mod ask;
pub(crate) mod listing;
pub(crate) mod model;
pub(crate) mod prompt;
pub(crate) mod session;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::hash::{BuildHasher as _, RandomState};
use std::io::{self, Write as _};
use std::ops::Range;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use dffy_core::annotation::Anchor;
use dffy_core::diff::{Hunk, LineKind};
use dffy_core::git::{Head, Repo};
use unicode_width::UnicodeWidthChar as _;

use self::model::text::{lines, one_line};
use crate::plugin::json::{self, Value};

const VERSION: usize = 1;

/// The `post_error` of a comment of yours the host deleted, kept for the
/// drafts in its thread.
const DELETED: &str = "deleted on the host";

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

/// A comment as the file holds it. Line comments have `start`, `end` and
/// `anchor`; file comments and replies have none of them. A general
/// comment, on the review rather than a file, has no path either.
#[derive(Debug, Clone, PartialEq, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the file's flags, each its own field"
)]
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
    /// Yours, but never posted until you make it yours again (`set` with an
    /// empty author): one that asked a local agent with `@<name>`, which
    /// on the host would mention someone, or set off the host's own bot;
    /// a question or follow-up to an agent, out of context there; and a
    /// note for Fix with agent.
    pub local_only: bool,
    /// Set by an integration on a thread's first comment: whether the host
    /// last said the thread is resolved. A `resolved` that differs is a
    /// resolve or reopen not sent yet; `None` for a thread the host doesn't
    /// resolve, or one no integration said of.
    pub host_resolved: Option<bool>,
    /// The id it had in the file it was moved or imported from, when that
    /// id is another's here: a move or import cut short and run again
    /// finds it by this instead of copying it twice.
    pub moved_from: Option<String>,
    /// When its suggestion was applied to the working tree, RFC 3339. Kept
    /// here only: never sent to a host.
    pub applied: Option<String>,
    /// The lines applying its suggestion replaced, a line break between
    /// each, which taking it back puts in place again. Kept here only, and
    /// cleared with `applied`.
    pub replaced: Option<String>,
    /// Fields dffy doesn't know, in file order, written back as read: on a
    /// comment from a host, the integration's own.
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
    /// A line comment's lines, `start` and `end` given together.
    pub start: Option<LineNumbers>,
    pub end: Option<LineNumbers>,
    pub start_side: Option<Option<Side>>,
    /// Removing it removes `replaced` too, unless that's given.
    pub applied: Option<Option<String>>,
    pub replaced: Option<Option<String>>,
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
        change: Box<Change>,
    },
    /// The comment's fields as given, a `ref` among them. They're merged
    /// into the comment with that ref field by field, so imports work on
    /// the JSON rather than a [`Comment`] that can't tell a field left out.
    Upsert {
        fields: Fields,
        reply_to: Option<Target>,
    },
    Rm(Target),
    /// The host no longer has the comments with these refs.
    Gone(Vec<String>),
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
            Self::Gone(_) => "gone",
            Self::Notice(_) => "notice",
            Self::Summary(_) => "summary",
        }
    }
}

/// The local review's file, the session pointer's, and the directory of
/// each review plugin's reviews, under the git directory every worktree
/// shares.
const LOCAL: &str = "dffy/comments.json";
const SESSION: &str = "dffy/session";
const REVIEWS: &str = "dffy/reviews";

/// Where review plugin `plugin`'s reviews' comments files are, one a
/// review, in the git directory every worktree shares, so they outlive a
/// worktree. `None` for paths compared outside git.
pub(crate) fn reviews_dir(repo: &Repo, plugin: &str) -> Option<PathBuf> {
    Some(shared_dir(repo)?.join(REVIEWS).join(plugin))
}

/// The comments file of the review a plugin calls `id`, in its plugin's
/// `dir`: the id, with every byte but lowercase ASCII letters, digits, `-`,
/// `_` and a `.` that doesn't start it as `%XX`, so any id is one file
/// name, and two ids that differ only in case are two files on a file
/// system that ignores case, as macOS's does. A name too long for its
/// lock file's name to fit in a file name (255 bytes) is its first 150
/// bytes, `-` and 16 hex digits of the id's digest.
pub(crate) fn review_file(dir: &Path, id: &str) -> PathBuf {
    let mut name = String::with_capacity(id.len() + 5);
    for (i, b) in id.bytes().enumerate() {
        if b.is_ascii_lowercase()
            || b.is_ascii_digit()
            || b == b'-'
            || b == b'_'
            || (b == b'.' && i > 0)
        {
            name.push(char::from(b));
        } else {
            let _ = write!(name, "%{b:02X}");
        }
    }
    // The file's siblings (`.lock`, `.post`, `.tmp`) add up to 5 bytes.
    if name.len() + ".json".len() + 5 > 255 {
        let digest = dffy_core::cache::digest(id.as_bytes()).to_hex().to_string();
        // ASCII throughout, so any byte is a boundary.
        name.truncate(150);
        name.push('-');
        name.push_str(&digest[..16]);
    }
    name.push_str(".json");
    dir.join(name)
}

/// `dffy comments import`: merges `from`, another comments file, such as
/// one a review plugin kept before dffy kept them, into the file at `to`,
/// under `to`'s lock. Each comment comes in with its id, drafts and all,
/// unless `to` has it already: by its `ref`, by its id on the same path
/// and time, or copied before under another id. Importing again adds
/// nothing, and a comment the host's import brought already stays as that
/// one has it. A comment whose id `to` has for another comment comes in
/// under a new id, its `moved_from` the old one. A reply joins its thread
/// whichever way `to` has the thread's first comment, wherever it is in
/// `from`. The summary comes too when `to` has none. `from` stays as it
/// is. Returns how many comments came.
pub(crate) fn import(from: &Path, to: &Path) -> Result<usize, String> {
    let bytes = fs::read(from).map_err(|e| failed("read", from, &e))?;
    let source = Review::parse(&bytes).map_err(|e| format!("{}: {e}", from.display()))?;
    update(to, |target| {
        if target.summary.is_none() {
            target.summary.clone_from(&source.summary);
        }
        let had = |c: &Comment, given: &Comment| {
            (given.reference.is_some() && c.reference == given.reference)
                || (c.id == given.id && c.path == given.path && c.created == given.created)
                || c.moved_from.as_ref() == Some(&given.id)
        };
        // The first comments of `from`'s threads, by their ids in `to`.
        let mut roots: HashMap<&str, String> = HashMap::new();
        let mut came = 0;
        let (firsts, replies): (Vec<&Comment>, Vec<&Comment>) = source
            .comments
            .iter()
            .partition(|c| c.in_reply_to.is_none());
        for given in firsts.into_iter().chain(replies) {
            if let Some(there) = target.comments.iter().find(|c| had(c, given)) {
                if given.in_reply_to.is_none() {
                    roots.insert(&given.id, there.id.clone());
                }
                continue;
            }
            let mut comment = given.clone();
            if target.comments.iter().any(|c| c.id == given.id) {
                comment.id = fresh_id(|id| target.comments.iter().any(|c| c.id == id));
                comment.moved_from = Some(given.id.clone());
            }
            match &given.in_reply_to {
                Some(root) => {
                    if let Some(at) = roots.get(root.as_str()) {
                        comment.in_reply_to = Some(at.clone());
                    }
                }
                None => {
                    roots.insert(&given.id, comment.id.clone());
                }
            }
            target.comments.push(comment);
            came += 1;
        }
        Ok(came)
    })
}

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

/// HEAD's commit, a full id; `None` on an unborn branch.
pub(crate) fn head_commit(repo: &Repo) -> Option<String> {
    repo.resolve_show("HEAD").ok().map(|(id, _)| id.to_string())
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
    parse_at(path, read_bytes(path)?.as_deref())
}

/// [`read`]'s bytes: `None` when the file doesn't exist.
pub(crate) fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("can't read {}: {e}", path.display())),
    }
}

/// [`read`]'s parse of the bytes [`read_bytes`] read from `path`.
pub(crate) fn parse_at(path: &Path, bytes: Option<&[u8]>) -> Result<Review, String> {
    bytes.map_or_else(
        || Ok(Review::default()),
        |bytes| Review::parse(bytes).map_err(|e| format!("{}: {e}", path.display())),
    )
}

/// Applies `ops` to the file under its lock, all or none, and returns the
/// id each one touched (`None` for a notice). Waits while another writer
/// holds the lock. Creates the file and its directory on first use.
pub(crate) fn apply(path: &Path, ops: Vec<Op>) -> Result<Vec<Option<String>>, String> {
    update(path, |comments| comments.apply(ops, SystemTime::now()))
}

/// The review was submitted on its host: none of its comments is visible
/// only to you any more. The file isn't written when none was.
pub(crate) fn submitted(path: &Path) -> Result<(), String> {
    if !read(path)?.comments.iter().any(|c| c.pending) {
        return Ok(());
    }
    update(path, |review| {
        for c in &mut review.comments {
            c.pending = false;
        }
        Ok(())
    })
}

/// Moves the threads whose first comments are `roots` from the file at
/// `from` to the one at `to`, each comment added there as [`Op::Add`]
/// adds one, with a new id, its replies after it; roots no longer in
/// `from` are skipped. `from` loses them only once `to` holds them: when
/// anything before writing `to` fails, neither file changes, and when
/// writing `from` after fails, they're in both. Locks `from`, then `to`.
/// They lose their `branch`: they're the review's now. Each copy's
/// `moved_from` is its id in `from`, so a move run again after one cut
/// short skips what `to` holds already. Returns how many comments moved.
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
                moved_from: Some(c.id.clone()),
                ..c.clone()
            })
            .collect();
        if moved.is_empty() {
            return Ok(0);
        }
        update(to, |target| {
            let mut places = Places::of(&target.comments);
            let copied = |target: &Review, c: &Comment| {
                (target.comments.iter())
                    .find(|t| t.moved_from == c.moved_from)
                    .map(|t| t.id.clone())
            };
            for root in moved.iter().filter(|c| c.in_reply_to.is_none()) {
                let id = match copied(target, root) {
                    Some(id) => id,
                    None => target.insert(root.clone(), None, now, &mut places)?,
                };
                let reply_to = Target::Id(id);
                for reply in moved
                    .iter()
                    .filter(|c| c.in_reply_to.as_ref() == Some(&root.id))
                {
                    if copied(target, reply).is_none() {
                        target.insert(reply.clone(), Some(&reply_to), now, &mut places)?;
                    }
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

/// Takes the post lock of the review whose comments file is `path`, which
/// a post holds from reading its drafts until it ends; `Err` when another
/// process holds it, never waiting. It isn't the file's own lock, which
/// each write takes, the post's own included.
pub(crate) fn lock_post(path: &Path) -> Result<File, String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| failed("create", dir, &e))?;
    }
    try_lock_post(&sibling(path, ".post"))?
        .ok_or_else(|| "another dffy is posting this review; post again once it's done".to_owned())
}

/// The post lock at `lock_path`; `None` while another process holds it.
fn try_lock_post(lock_path: &Path) -> Result<Option<File>, String> {
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)
        .map_err(|e| failed("open", lock_path, &e))?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => Err(failed("lock", lock_path, &e)),
    }
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

/// Operations as JSON in, the answer's JSON out, as `dffy comments apply`
/// and a review plugin's `comments/apply` give them. `branch`, for the
/// local review's file, goes to each thread `add` starts without one.
pub(crate) fn apply_json(
    path: &Path,
    input: &[u8],
    branch: Option<&str>,
) -> Result<String, String> {
    Ok(answer(&apply(path, parse_ops(input, branch)?)?))
}

/// `dffy comments apply`: [`apply_json`], except that operations changing
/// what a comment says are refused while the review is being posted, as
/// the post read its drafts before; they hold the post lock while they
/// write, so a post doesn't start in between. A file never posted has no
/// post lock to take. `author`, `--as`'s, writes each `add` that names no
/// author, which would otherwise be yours.
pub(crate) fn apply_given(
    path: &Path,
    input: &[u8],
    branch: Option<&str>,
    author: Option<&str>,
) -> Result<String, String> {
    let mut ops = parse_ops(input, branch)?;
    if let Some(author) = author {
        for op in &mut ops {
            if let Op::Add { comment, .. } = op
                && comment.author.is_empty()
            {
                author.clone_into(&mut comment.author);
            }
        }
    }
    let rewrites = ops.iter().any(|op| {
        matches!(op, Op::Set { change, .. } if change.text.is_some() || change.suggestion.is_some())
    });
    let lock_path = sibling(path, ".post");
    let _posting = if rewrites && lock_path.exists() {
        let held = try_lock_post(&lock_path)?;
        Some(held.ok_or("the review is being posted; change its drafts once it's done")?)
    } else {
        None
    };
    Ok(answer(&apply(path, ops)?))
}

/// What [`apply_json`] answers: the id each operation touched.
fn answer(ids: &[Option<String>]) -> String {
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
    out
}

fn lock_path(path: &Path) -> PathBuf {
    sibling(path, ".lock")
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Where each comment of a file being changed is, by id and by ref, so a
/// batch of operations, an import's thousands among them, looks each up
/// without a scan. The operations that add a comment or change a ref
/// keep it; those that remove comments make it again.
struct Places {
    ids: HashMap<String, usize>,
    refs: HashMap<String, usize>,
}

impl Places {
    fn of(comments: &[Comment]) -> Self {
        let mut places = Self {
            ids: HashMap::with_capacity(comments.len()),
            refs: HashMap::with_capacity(comments.len()),
        };
        // The first of a repeated id or ref, as a scan from the start
        // would find, in a file written by hand.
        for (i, c) in comments.iter().enumerate() {
            places.ids.entry(c.id.clone()).or_insert(i);
            if let Some(reference) = &c.reference {
                places.refs.entry(reference.clone()).or_insert(i);
            }
        }
        places
    }

    fn find(&self, target: &Target) -> Result<usize, String> {
        match target {
            Target::Id(id) => {
                (self.ids.get(id).copied()).ok_or_else(|| format!("comment {id} doesn't exist"))
            }
            Target::Ref(reference) => (self.refs.get(reference).copied())
                .ok_or_else(|| format!("no comment has ref {reference}")),
        }
    }
}

/// An id that isn't `taken`. Random, so an id is never handed out again
/// after a removal.
fn fresh_id(taken: impl Fn(&str) -> bool) -> String {
    loop {
        let id = format!("{:016x}", RandomState::new().hash_one(()));
        if !taken(&id) {
            return id;
        }
    }
}

impl Review {
    /// The drafts a review session's post sends, in file order: each
    /// [`Comment::draft`], a reply only in a thread that
    /// [`Comment::takes_replies`].
    pub(crate) fn drafts(&self) -> impl Iterator<Item = &Comment> {
        let closed: HashSet<&str> = (self.comments.iter())
            .filter(|c| c.in_reply_to.is_none() && !c.takes_replies())
            .map(|c| c.id.as_str())
            .collect();
        (self.comments.iter()).filter(move |c| {
            c.draft() && (c.in_reply_to.as_deref()).is_none_or(|root| !closed.contains(root))
        })
    }

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
        let mut places = Places::of(&next.comments);
        let mut ids = Vec::with_capacity(ops.len());
        for (i, op) in ops.into_iter().enumerate() {
            let name = op.name();
            let id = next
                .apply_one(op, now, &mut places)
                .map_err(|e| format!("operation {i} ({name}): {e}"))?;
            ids.push(id);
        }
        *self = next;
        Ok(ids)
    }

    fn apply_one(
        &mut self,
        op: Op,
        now: SystemTime,
        places: &mut Places,
    ) -> Result<Option<String>, String> {
        match op {
            Op::Add { comment, reply_to } => self
                .insert(*comment, reply_to.as_ref(), now, places)
                .map(Some),
            Op::Set { target, change } => self.set(&target, *change, places).map(Some),
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
                let Some(&i) = places.refs.get(reference) else {
                    let comment = Comment::from_new_fields(fields)?;
                    return (self.insert(comment, reply_to.as_ref(), now, places)).map(Some);
                };
                // A resolve or reopen not sent is yours: an import that read
                // the thread before it was made changes only what the host
                // says of it.
                let unsent = self.comments[i].resolve_unsent();
                let mut merged = self.comments[i].to_fields();
                for (key, value) in fields {
                    if unsent && key == "resolved" {
                        continue;
                    }
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
                    self.join_thread(&mut comment, target, places)?;
                    let id = &comment.id;
                    if comment.in_reply_to.as_ref() == Some(id) {
                        return Err(format!("comment {id} can't reply in its own thread"));
                    }
                    // Replies hang off the first comment alone, so one that
                    // was a reply has none.
                    if self.comments[i].in_reply_to.is_none()
                        && (self.comments.iter()).any(|c| c.in_reply_to.as_ref() == Some(id))
                    {
                        return Err(format!(
                            "comment {id} has replies, so it can't become a reply itself"
                        ));
                    }
                }
                self.check(&comment, Some(i), places)?;
                self.comments[i] = comment;
                Ok(Some(self.comments[i].id.clone()))
            }
            Op::Rm(target) => {
                let id = self.comments[places.find(&target)?].id.clone();
                self.comments
                    .retain(|c| c.id != id && c.in_reply_to.as_ref() != Some(&id));
                *places = Places::of(&self.comments);
                Ok(Some(id))
            }
            Op::Gone(refs) => {
                self.gone(&refs.iter().map(String::as_str).collect());
                *places = Places::of(&self.comments);
                Ok(None)
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

    /// [`Op::Gone`]: a thread whose comments are all gone goes; a gone
    /// reply goes alone. A gone first comment, general ones included, stays
    /// while its thread holds replies that aren't gone, such as your
    /// drafts, so they aren't lost: posted no longer, without its ref and
    /// what the host said of it, the integration's fields included. One of
    /// yours would be a draft again, which a post sends as a new thread, so
    /// it says it was deleted on the host, as one the host refused does.
    fn gone(&mut self, refs: &HashSet<&str>) {
        let is_gone = |c: &Comment| c.reference.as_deref().is_some_and(|r| refs.contains(r));
        let removed: HashSet<String> = (self.comments.iter())
            .filter(|root| root.in_reply_to.is_none() && is_gone(root))
            .filter(|root| {
                (self.comments.iter())
                    .filter(|c| c.in_reply_to.as_ref() == Some(&root.id))
                    .all(is_gone)
            })
            .map(|root| root.id.clone())
            .collect();
        self.comments.retain(|c| {
            let root = c.in_reply_to.as_ref().unwrap_or(&c.id);
            !removed.contains(root) && (c.in_reply_to.is_none() || !is_gone(c))
        });
        for c in self.comments.iter_mut().filter(|c| is_gone(c)) {
            c.reference = None;
            c.url = None;
            c.outdated = false;
            c.pending = false;
            c.host_resolved = None;
            c.extra.clear();
            if c.author.is_empty() {
                c.post_error = Some(DELETED.to_owned());
            }
        }
    }

    /// [`Op::Set`]: returns the id of the comment changed.
    #[expect(clippy::too_many_lines, reason = "a step per field a set changes")]
    fn set(
        &mut self,
        target: &Target,
        change: Change,
        places: &mut Places,
    ) -> Result<String, String> {
        let i = places.find(target)?;
        if change.anchor.as_ref().is_some_and(|a| a.lines.is_empty()) {
            return Err("an anchor needs at least one line".to_owned());
        }
        if change.author.as_ref().is_some_and(|a| !a.is_empty()) {
            return Err(
                "`author` can only be set to \"\", which makes the comment yours".to_owned(),
            );
        }
        let named = &self.comments[i];
        if change.reference == Some(None) && named.path.is_empty() && named.in_reply_to.is_none() {
            return Err("a general comment keeps its ref".to_owned());
        }
        let moves = change.start.is_some() || change.end.is_some() || change.start_side.is_some();
        if moves && named.end.is_none() {
            return Err("only a line comment has lines to move".to_owned());
        }
        if change.start.is_some() != change.end.is_some() {
            return Err("`start` and `end` are set together".to_owned());
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
        // Two posts of one draft, as from two viewers: the second's ref would
        // leave the first's comment on the host unknown here.
        if let (Some(Some(new)), Some(old)) = (&change.reference, &comment.reference)
            && new != old
        {
            return Err(format!(
                "comment {} is posted as {old} already, so it can't be {new}",
                comment.id
            ));
        }
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
            comment.applied = None;
            comment.replaced = None;
        }
        if let Some(applied) = change.applied {
            comment.replaced = comment.replaced.take().filter(|_| applied.is_some());
            comment.applied = applied;
        }
        if let Some(replaced) = change.replaced {
            comment.replaced = replaced;
        }
        comment.start = change.start.or(comment.start);
        comment.end = change.end.or(comment.end);
        if let Some(start_side) = change.start_side {
            comment.start_side = start_side;
        }
        if let Some(author) = change.author {
            comment.author = author;
            comment.local_only = false;
        }
        if let Some(resolved) = change.resolved {
            comment.resolved = resolved;
        }
        let old_ref = change
            .reference
            .is_some()
            .then(|| comment.reference.clone());
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
        self.check(&self.comments[i], Some(i), places)?;
        if let Some(old) = old_ref {
            if let Some(old) = old {
                places.refs.remove(&old);
            }
            if let Some(new) = &self.comments[i].reference {
                places.refs.insert(new.clone(), i);
            }
        }
        Ok(self.comments[i].id.clone())
    }

    fn insert(
        &mut self,
        mut comment: Comment,
        reply_to: Option<&Target>,
        now: SystemTime,
        places: &mut Places,
    ) -> Result<String, String> {
        comment.in_reply_to = None;
        if let Some(target) = reply_to {
            self.join_thread(&mut comment, target, places)?;
        }
        self.check(&comment, None, places)?;
        if comment.created.is_empty() {
            comment.created = timestamp(now);
        }
        comment.id = fresh_id(|id| places.ids.contains_key(id));
        let at = self.comments.len();
        places.ids.insert(comment.id.clone(), at);
        if let Some(reference) = &comment.reference {
            places.refs.insert(reference.clone(), at);
        }
        self.comments.push(comment);
        Ok(self.comments[at].id.clone())
    }

    /// Makes `comment` a reply in `target`'s thread: it takes the thread's
    /// place and branch and has none of its own.
    fn join_thread(
        &self,
        comment: &mut Comment,
        target: &Target,
        places: &Places,
    ) -> Result<(), String> {
        let parent = &self.comments[places.find(target)?];
        let root_id = parent.in_reply_to.as_ref().unwrap_or(&parent.id);
        let root = (places.ids.get(root_id)).map_or(parent, |&at| &self.comments[at]);
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
    fn check(&self, comment: &Comment, at: Option<usize>, places: &Places) -> Result<(), String> {
        if let Some(reference) = &comment.reference
            && let Some(&other) = places.refs.get(reference)
            && Some(other) != at
        {
            return Err(format!(
                "ref {reference} is already on comment {}",
                self.comments[other].id
            ));
        }
        if comment.anchor.is_some() && (comment.in_reply_to.is_some() || comment.end.is_none()) {
            return Err("only a line comment has an anchor".to_owned());
        }
        if comment.in_reply_to.is_some() {
            return Ok(());
        }
        // A general comment comes only from an integration, which names it
        // by its ref; one the host deleted under replies of yours stays
        // without it ([`Self::gone`]).
        if comment.path.is_empty() {
            if comment.reference.is_none() && at.is_none() {
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

    /// The open threads of `branches` that are yours, in [`threads`] order,
    /// each its first comment then its replies: those you started, or
    /// replied in.
    pub(crate) fn open_threads_of_yours(&self, branches: Branches) -> Vec<Vec<&Comment>> {
        let select = Select {
            resolved: Some(false),
            branches,
            ..Select::default()
        };
        let mut out: Vec<Vec<&Comment>> = Vec::new();
        for c in threads(self.selected(&select)) {
            match out.last_mut() {
                Some(thread) if c.in_reply_to.as_ref() == Some(&thread[0].id) => thread.push(c),
                _ => out.push(vec![c]),
            }
        }
        out.retain(|thread| thread.iter().any(|c| c.author.is_empty()));
        out
    }

    /// The thread comment `id` is in: its first comment, then its replies
    /// in file order. `None` when no comment has that id.
    pub(crate) fn thread(&self, id: &str) -> Option<Vec<&Comment>> {
        let named = self.comments.iter().find(|c| c.id == id)?;
        let root = (named.in_reply_to.as_deref())
            .and_then(|root| self.comments.iter().find(|c| c.id == root))
            .unwrap_or(named);
        let replies = (self.comments.iter())
            .filter(|c| c.in_reply_to.as_ref() == Some(&root.id) && c.path == root.path);
        Some(std::iter::once(root).chain(replies).collect())
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

    /// Yours, without a ref and not kept local: what a review session's
    /// post sends, a reply only in a thread that [`Comment::takes_replies`].
    pub(crate) fn draft(&self) -> bool {
        self.reference.is_none() && self.author.is_empty() && !self.local_only
    }

    /// A thread's first comment that a post may put a draft reply under:
    /// any but one kept local, which isn't on the host and won't be until
    /// you make it yours. Under another not on the host, the review
    /// plugin's plan says why the reply can't go.
    pub(crate) fn takes_replies(&self) -> bool {
        !self.local_only
    }

    /// A posted thread's first comment resolved or reopened here since the
    /// host last said otherwise: what a review plugin's `post` sends.
    pub(crate) fn resolve_unsent(&self) -> bool {
        self.reference.is_some()
            && self.in_reply_to.is_none()
            && self
                .host_resolved
                .is_some_and(|on_host| on_host != self.resolved)
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
                "moved_from" => c.moved_from = string(&key, value)?,
                "applied" => c.applied = string(&key, value)?,
                "replaced" => c.replaced = string(&key, value)?,
                "host_resolved" => {
                    c.host_resolved = match value {
                        Value::Null => None,
                        Value::Bool(b) => Some(b),
                        _ => return Err("`host_resolved` must be true or false".to_owned()),
                    };
                }
                "outdated" | "pending" | "local_only" => {
                    let flag = match value {
                        Value::Null => false,
                        Value::Bool(b) => b,
                        _ => return Err(format!("`{key}` must be true or false")),
                    };
                    match key.as_str() {
                        "outdated" => c.outdated = flag,
                        "pending" => c.pending = flag,
                        _ => c.local_only = flag,
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
        put(
            &mut f,
            "local_only",
            self.local_only.then_some(Value::Bool(true)),
        );
        put(&mut f, "host_resolved", self.host_resolved.map(Value::Bool));
        put(&mut f, "moved_from", self.moved_from.as_deref().map(text));
        put(&mut f, "applied", self.applied.as_deref().map(text));
        put(&mut f, "replaced", self.replaced.as_deref().map(text));
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
            change: Box::new(change(take(&mut fields, "fields"))?),
        }),
        Some("upsert") => {
            let (fields, reply_to) = comment_fields(take(&mut fields, "comment"))?;
            Ok(Op::Upsert { fields, reply_to })
        }
        Some("rm") => Ok(Op::Rm(target(
            take(&mut fields, "id"),
            take(&mut fields, "ref"),
        )?)),
        Some("gone") => match take(&mut fields, "refs") {
            Some(Value::Array(refs)) => refs
                .into_iter()
                .map(|r| match r {
                    Value::String(r) => Ok(r),
                    _ => Err("each of refs must be a string".to_owned()),
                })
                .collect::<Result<_, _>>()
                .map(Op::Gone),
            _ => Err("needs refs, a list of strings".to_owned()),
        },
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
            "start" => {
                change.start = Some(line_numbers(&key, &value)?.ok_or("`start` can't be null")?);
            }
            "end" => change.end = Some(line_numbers(&key, &value)?.ok_or("`end` can't be null")?),
            "start_side" => change.start_side = Some(side(&key, value)?),
            "applied" => change.applied = Some(string(&key, value)?),
            "replaced" => change.replaced = Some(string(&key, value)?),
            _ => {
                return Err(format!(
                    "`{key}` can't be set; set changes text, resolved, ref, anchor, author, \
                     suggestion, branch, pending, post_error, start, end, start_side, applied \
                     and replaced"
                ));
            }
        }
    }
    Ok(change)
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

/// A comment's author as said, on one line: `you` for an empty one, and for
/// no other, as agents take what's headed `you` for the user's words.
pub(crate) fn who(author: &str) -> Cow<'_, str> {
    if author.is_empty() {
        return Cow::Borrowed("you");
    }
    let shown = one_line(author);
    if reads_as_you(author) {
        Cow::Owned(format!("someone named {}", shown.trim()))
    } else if shown == author {
        Cow::Borrowed(author)
    } else {
        Cow::Owned(shown)
    }
}

/// Whether `name`'s first word reads as `you`: in any case, with characters
/// that don't show, or with letters of other alphabets that look the same.
pub(crate) fn reads_as_you(name: &str) -> bool {
    let shows = |c: &char| {
        // Takes columns, though there's nothing to see.
        let blank = matches!(c, '\u{115f}' | '\u{1160}' | '\u{3164}' | '\u{ffa0}');
        c.width().is_some_and(|w| w > 0) && !blank
    };
    // Cyrillic's, Greek's, Armenian's and the fullwidth letters; not every
    // lookalike Unicode has.
    let mut letters = (name.chars().filter(shows)).map(|c| match c {
        'Y' | '\u{443}' | '\u{423}' | '\u{4af}' | '\u{4ae}' | '\u{3a5}' | '\u{ff39}'
        | '\u{ff59}' => 'y',
        'O' | '0' | '\u{43e}' | '\u{41e}' | '\u{3bf}' | '\u{39f}' | '\u{585}' | '\u{555}'
        | '\u{ff2f}' | '\u{ff4f}' => 'o',
        'U' | '\u{3c5}' | '\u{57d}' | '\u{54d}' | '\u{ff35}' | '\u{ff55}' => 'u',
        c => c,
    });
    letters.find(|c| c.is_alphanumeric()) == Some('y')
        && letters.next() == Some('o')
        && letters.next() == Some('u')
        && letters.next().is_none_or(|c| !c.is_alphanumeric())
}

/// `comments`, in [`threads`] order, as Markdown: `summary` first when
/// there's one, then a heading per thread with its place, the commented
/// lines as they were in a code block, and each comment's author and time
/// over its text, which is Markdown already. The lines and the comments are
/// quoted, so nothing in them reads as a heading of dffy's.
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
                    out.push_str(&one_line(word));
                }
            } else {
                let path = one_line(&comment.path);
                let ticks = fence(&path, 1);
                let pad = if path.starts_with('`') || path.ends_with('`') {
                    " "
                } else {
                    ""
                };
                let _ = write!(out, "{ticks}{pad}{path}{pad}{ticks}");
            }
            match (&comment.in_reply_to, comment.range()) {
                (Some(root), _) => {
                    let _ = write!(out, ", reply in thread {}", one_line(root));
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
                quoted(&mut out, &code_block(&anchor.lines.join("\n")));
            }
        }
        let (author, created) = (who(&comment.author), one_line(&comment.created));
        let _ = write!(out, "**{author}**, {created}\n\n");
        let mut body = comment.text.trim_end().to_owned();
        if let Some(suggestion) = &comment.suggestion {
            let _ = write!(body, "\n\nSuggested change:\n\n{}", code_block(suggestion));
        }
        quoted(&mut out, &body);
    }
    out.truncate(out.trim_end().len());
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// A fenced code block of `text`, its fence longer than any run of
/// backticks in it.
fn code_block(text: &str) -> String {
    let ticks = fence(text, 3);
    format!("{ticks}\n{text}\n{ticks}")
}

/// Pushes `text` as a Markdown quote.
fn quoted(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    for line in lines(text) {
        let _ = match line.as_str() {
            "" => out.write_str(">\n"),
            line => writeln!(out, "> {line}"),
        };
    }
    out.push('\n');
}

/// Backticks, at least `min` and one more than the longest run in `text`.
fn fence(text: &str, min: usize) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat((longest + 1).max(min))
}

/// `t` in RFC 3339, UTC, to the second.
pub(crate) fn timestamp(t: SystemTime) -> String {
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

//! Review comments in the viewer: the comments file as last read, the
//! threads placed from it on each file, and the panel listing them.
//!
//! Threads are placed when a file's diff loads or its comments change,
//! never while drawing. Files with comments are diffed in the background
//! (see [`Comments::pending`]) so every thread finds its row, or is known
//! not to. The diff draws a placed thread under its last line, a comment
//! on the whole file under the file's first row, and one whose lines are
//! gone under its last row; [`thread_rows`] writes what goes there.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dffy_core::bstr::BString;
use dffy_core::diff::{Hunk, LineKind, Text};
use dffy_core::document::{File, Side};
use dffy_core::git::{FileChange, FileDiff, Status};
use dffy_core::highlight;
use dffy_core::view::{Attributes, Run, Spans};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::markdown::{self, Body};
use super::textbox::TextBox;
use super::view::{Entry, paint};
use super::{fuzzy, tree};
use crate::keys::{Action as Act, Keys};
use crate::render::{Canvas, escape};
use crate::review::{self, Anchor, Comment, LineNumbers, Op, Review, who};
use crate::term::{Grid, Key, Mods};
use crate::theme::{Ink, Role, Theme};

/// How a thread shows, least pressing first: a thread with a draft in it
/// is a draft, though its root is posted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Look {
    Resolved,
    Open,
    Draft,
}

impl Look {
    /// Shapes differ as well as colors: without color the theme is
    /// attributes only.
    pub(crate) fn glyph(self) -> &'static str {
        match self {
            Self::Resolved => "○",
            Self::Open => "●",
            Self::Draft => "◆",
        }
    }

    pub(crate) fn role(self) -> Role {
        match self {
            Self::Resolved => Role::Dim,
            Self::Open => Role::Hint,
            Self::Draft => Role::Warn,
        }
    }
}

/// Where a comment stands with the host. Only a review session's file
/// (one a review plugin keeps) is posted; in any other, nothing is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    /// It has a `ref`: the host's, so its text doesn't change here.
    Posted,
    /// Yours, in a review session, not posted yet: what `P` posts.
    Draft,
    /// Someone else's, an agent's, in a review session, without a `ref`:
    /// `P` leaves it, since it would post under your name.
    Unposted,
    /// Outside a review session.
    Kept,
}

/// Where a thread shows on its file's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum At {
    /// On the whole file: under the file's first row.
    File,
    /// Under the row showing the last of `lines` (0-based) of `side`, in
    /// the line diff (a rendered view maps its rows to them).
    Line { side: Side, lines: Range<usize> },
    /// Its lines weren't found: under the file's last row.
    Lost,
    /// A general comment, on the review rather than a file: on the review's
    /// conversation page, with the integration's words for what it is.
    Conversation {
        kind: Option<String>,
        /// Escaped, ready to draw.
        label: Option<String>,
    },
}

/// A comment as its thread shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Note {
    pub id: String,
    /// Empty means you.
    pub author: String,
    pub created: String,
    /// Markdown.
    pub text: String,
    /// [`markdown::summary`] of the text.
    pub summary: String,
    pub suggestion: Option<String>,
    pub standing: Standing,
    /// On the host, but only for you: in a review not submitted yet.
    pub pending: bool,
    /// For a draft, what posting it does, when that's news: refused
    /// before, waiting for its lines, or going on the whole file. Escaped.
    pub posting: Option<(Role, String)>,
}

impl Note {
    /// On the host: its text can't change here.
    pub(crate) fn posted(&self) -> bool {
        self.standing == Standing::Posted
    }
}

/// A thread placed on its file, as the diff draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Thread {
    /// The id of its first comment.
    pub root: String,
    pub at: At,
    pub look: Look,
    pub resolved: bool,
    pub outdated: bool,
    /// The first comment on its host.
    pub url: Option<String>,
    /// The first comment, then the replies in the order they were added.
    pub notes: Vec<Note>,
    /// The commented lines' text, which a suggestion replaces.
    pub lines: Vec<String>,
    /// For a thread whose lines weren't found, the code it was written on.
    pub snippet: Option<Snippet>,
    /// A draft of the local review's file, shown on a review session's
    /// diff.
    pub local: bool,
    /// The branch of a thread written on a branch other than the current
    /// one, which only the comments panel lists. Escaped.
    pub branch: Option<String>,
}

impl Thread {
    /// Whether it holds someone else's comment not posted, as an agent's,
    /// that `a` makes yours.
    pub(crate) fn unposted(&self) -> bool {
        self.notes.iter().any(|n| n.standing == Standing::Unposted)
    }
}

/// The code a thread was written on, drawn at the top of its box when its
/// lines weren't found: the commented lines between their context, with
/// their numbers in the file as it was then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snippet {
    /// What the heading says instead of `lines not found`: `line 145 not
    /// found`, `old lines 140–141 not found`.
    pub heading: String,
    pub rows: Vec<CodeRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodeRow {
    /// 1-based.
    pub number: usize,
    /// `+` or `-` on a commented line, a space on context.
    pub sign: &'static str,
    /// The text, tabs expanded and escaped, in its inks: context dimmed.
    pub pieces: Vec<(Ink, String)>,
}

/// The text box open in the diff, and what its text becomes.
pub(crate) struct Writing {
    pub at: Writes,
    pub text: TextBox,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Writes {
    /// A new thread on `lines` (0-based, in the line diff) of `side`.
    New { side: Side, lines: Range<usize> },
    /// A new thread on the whole file shown, under its first row.
    File,
    /// A reply, at the end of the thread with this root.
    Reply(String),
    /// Comment `id` of the thread with root `root`, in place of its text.
    Edit { root: String, id: String },
}

/// Where a path's threads went, against one version of its entry.
struct Placed {
    version: u64,
    threads: Arc<[Thread]>,
    /// Local drafts whose anchor matched nowhere: out of view.
    hidden: Vec<String>,
}

/// What the text box's or the editor's text becomes, kept while either is
/// open.
#[derive(Clone)]
pub(super) enum Compose {
    New(Box<Comment>),
    /// A reply to the thread with this root.
    Reply(String),
    Edit {
        id: String,
        before: String,
    },
}

impl Compose {
    /// The operation saving `text`, and what the status bar says after;
    /// `None` when there's nothing to save. A draft edited down to nothing
    /// is deleted, as `d d` would. A comment on lines that ends in a
    /// `suggestion` fenced block, as [`with_suggestion`] writes it, keeps
    /// the block's lines as its suggestion, as they were typed; a new one
    /// suggesting the lines as they are, with nothing said, is nothing.
    pub(super) fn into_op(self, text: &str) -> Option<(Op, &'static str)> {
        let lines = match &self {
            Self::New(comment) => comment.end.is_some(),
            Self::Edit { .. } => true,
            Self::Reply(_) => false,
        };
        let (text, mut suggestion) = if lines {
            split_suggestion(text)
        } else {
            (text, None)
        };
        let text = text.trim();
        if let Self::New(comment) = &self
            && text.is_empty()
            && suggestion == comment.anchor.as_ref().map(|a| a.lines.join("\n"))
        {
            suggestion = None;
        }
        if text.is_empty() && suggestion.is_none() {
            return match self {
                Self::Edit { id, .. } => Some((Op::Rm(review::Target::Id(id)), "draft deleted")),
                Self::New(_) | Self::Reply(_) => None,
            };
        }
        Some(match self {
            Self::New(mut comment) => {
                text.clone_into(&mut comment.text);
                comment.suggestion = suggestion;
                (
                    Op::Add {
                        comment,
                        reply_to: None,
                    },
                    "comment saved",
                )
            }
            Self::Reply(root) => (
                Op::Add {
                    comment: Box::new(Comment {
                        text: text.to_owned(),
                        ..Comment::default()
                    }),
                    reply_to: Some(review::Target::Id(root)),
                },
                "reply saved",
            ),
            Self::Edit { before, .. }
                if split_suggestion(&before) == (text, suggestion.clone()) =>
            {
                return None;
            }
            Self::Edit { id, .. } => (
                Op::Set {
                    target: review::Target::Id(id),
                    change: review::Change {
                        text: Some(text.to_owned()),
                        suggestion: Some(suggestion),
                        ..review::Change::default()
                    },
                },
                "comment saved",
            ),
        })
    }
}

/// `text` with `suggestion` after it as a closing `suggestion` fenced
/// block, the way code hosts write one, to edit both in one box.
pub(super) fn with_suggestion(text: &str, suggestion: Option<&str>) -> String {
    let Some(suggestion) = suggestion else {
        return text.to_owned();
    };
    let text = text.trim_end();
    let gap = if text.is_empty() { "" } else { "\n\n" };
    format!("{text}{gap}```suggestion\n{suggestion}\n```")
}

/// `text` without the closing `suggestion` fenced block [`with_suggestion`]
/// writes, trimmed, and the block's lines, without their last line break.
fn split_suggestion(text: &str) -> (&str, Option<String>) {
    text.trim_end()
        .strip_suffix("```")
        .and_then(|t| t.rsplit_once("```suggestion\n"))
        .filter(|(before, _)| before.is_empty() || before.ends_with('\n'))
        .map_or_else(
            || (text.trim(), None),
            |(before, block)| {
                let block = block.strip_suffix('\n').unwrap_or(block);
                (before.trim(), Some(block.to_owned()))
            },
        )
}

/// The comments file shown and, on a review session's, the drafts of the
/// local review's file (your comments without a ref, and their threads).
/// A local draft shows only where it places on this diff, marked `local`;
/// edits, replies and deletes in its thread go to the local file, and
/// posting moves it to the session's first (see [`Comments::move_local`]).
///
/// The local review's file holds every branch's threads: only those on
/// `branch` and on none are shown, the rest kept apart for the panel's
/// "other branches" group. A review session's own file isn't split so.
pub(super) struct Comments {
    path: PathBuf,
    /// The local review's file, when it isn't `path`.
    local: Option<PathBuf>,
    /// `path` is a review session's: your comments without a ref in it
    /// are drafts.
    session: bool,
    /// The current branch; `None` on a detached HEAD.
    branch: Option<String>,
    /// New threads go to `local`, not `path`: on the review plain `dffy`
    /// found, where they're the working tree's until posted.
    new_local: bool,
    /// `path`'s comments, then `local`'s drafts.
    review: Review,
    /// The ids of the comments in `review` from `local`.
    from_local: HashSet<String>,
    /// The threads of the local review's file on other branches, roots
    /// first, as the file has them; not in `review`.
    others: Vec<Comment>,
    /// What posting each draft does, by id, when that's news: the words
    /// its heading says.
    plans: HashMap<String, (Role, String)>,
    /// Renamed files in the list, by new path: their old path.
    renamed: HashMap<BString, BString>,
    /// Each path's comments, replies included, as indices into `review`.
    by_path: HashMap<BString, Vec<usize>>,
    placed: HashMap<BString, Placed>,
    /// Where each commented path is in the list, when it is.
    located: HashMap<BString, usize>,
    /// What stat said of `path` and `local` just before the last read.
    stamps: Vec<Option<Stamp>>,
    /// Changes whenever the threads listed or placed may have: what the
    /// panel's list is kept against.
    generation: u64,
    /// The general comments' threads, made when the conversation page or
    /// the panel first shows them after a read.
    conversation: OnceCell<Arc<[Thread]>>,
    /// [`Comments::counts`] at the generation it was counted at: the status
    /// bar asks every frame.
    counted: Cell<Option<(u64, (usize, usize))>>,
    /// [`Comments::host_pending`] at the generation it was counted at: the
    /// status bar asks every frame.
    pending_counted: Cell<Option<(u64, usize)>>,
    /// [`Comments::tally`] of every path, at the generation it was counted
    /// at: the tree asks for each row it draws, every frame.
    tallied: RefCell<Option<(u64, Tallies)>>,
    /// How many general comments [`Comments::conversation`] shows, for the
    /// tree's row.
    general_len: usize,
}

/// Modification time (seconds, nanoseconds), size and inode.
type Stamp = (i64, i64, u64, u64);

/// [`Comments::tally`] by path.
type Tallies = HashMap<BString, (usize, Look)>;

impl Comments {
    /// No comments until [`Comments::reload`] reads `path`, and `local`'s
    /// drafts when it's another file. `branch` is the current branch.
    pub(super) fn new(
        path: PathBuf,
        local: Option<PathBuf>,
        session: bool,
        branch: Option<String>,
    ) -> Self {
        // Compared as the files they name: posting moves drafts from `local`
        // to `path`, and one file under two spellings would wait forever on
        // its own lock. A file not written yet is named by its directory.
        let real = |p: &Path| {
            p.canonicalize()
                .ok()
                .or_else(|| Some(p.parent()?.canonicalize().ok()?.join(p.file_name()?)))
        };
        Self {
            local: local.filter(|l| *l != path && real(l).is_none_or(|l| Some(l) != real(&path))),
            path,
            session,
            branch,
            new_local: false,
            review: Review::default(),
            from_local: HashSet::new(),
            others: Vec::new(),
            plans: HashMap::new(),
            renamed: HashMap::new(),
            by_path: HashMap::new(),
            placed: HashMap::new(),
            located: HashMap::new(),
            stamps: Vec::new(),
            generation: 0,
            conversation: OnceCell::new(),
            counted: Cell::new(None),
            pending_counted: Cell::new(None),
            tallied: RefCell::new(None),
            general_len: 0,
        }
    }

    /// The files read: the comments file, then the local review's.
    pub(super) fn sources(&self) -> Vec<PathBuf> {
        std::iter::once(&self.path)
            .chain(&self.local)
            .cloned()
            .collect()
    }

    /// Whether `c` is a local draft that didn't place, or isn't placed
    /// yet: out of view.
    fn hides(&self, c: &Comment) -> bool {
        // Checked per comment as the status bar draws.
        !self.from_local.is_empty()
            && self.from_local.contains(&c.id)
            && self
                .placed
                .get(c.path.as_bytes())
                .is_none_or(|p| p.hidden.contains(c.in_reply_to.as_ref().unwrap_or(&c.id)))
    }

    pub(super) fn notice(&self) -> Option<&str> {
        self.review.notice.as_deref()
    }

    /// Takes the file as a review session's, or not. Returns the paths
    /// whose threads are to be placed again for it, none when it didn't
    /// change.
    pub(super) fn set_session(&mut self, session: bool) -> Vec<BString> {
        if self.session == session {
            return Vec::new();
        }
        self.session = session;
        self.generation += 1;
        self.placed.clear();
        self.conversation = OnceCell::new();
        self.general_len = self.general().count();
        self.by_path.keys().cloned().collect()
    }

    fn standing(&self, c: &Comment) -> Standing {
        match (&c.reference, self.session, c.author.is_empty()) {
            (Some(_), ..) => Standing::Posted,
            (None, false, _) => Standing::Kept,
            (None, true, true) => Standing::Draft,
            (None, true, false) => Standing::Unposted,
        }
    }

    /// [`Comments::reload`], unless stat says neither file changed since
    /// the last read, as it doesn't after dffy's own writes, read back as
    /// they're made.
    pub(super) fn refresh(&mut self) -> Result<Option<Vec<BString>>, String> {
        if self.stamped() == self.stamps {
            return Ok(None);
        }
        self.reload()
    }

    fn stamped(&self) -> Vec<Option<Stamp>> {
        let stamp = |path: &PathBuf| {
            std::fs::metadata(path)
                .ok()
                .map(|m| (m.mtime(), m.mtime_nsec(), m.size(), m.ino()))
        };
        std::iter::once(&self.path)
            .chain(&self.local)
            .map(stamp)
            .collect()
    }

    /// Reads the files again. Returns the paths whose comments changed,
    /// `None` when nothing did; on an error in either the last read stays.
    pub(super) fn reload(&mut self) -> Result<Option<Vec<BString>>, String> {
        // Taken first: a write landing during the read then shows as a
        // change next time. Kept on an error too, so a broken file is
        // reported once, not on every event.
        self.stamps = self.stamped();
        let mut review = review::read(&self.path)?;
        let mut from_local = HashSet::new();
        let mut others = Vec::new();
        if !self.session {
            let (here, elsewhere) =
                by_branch(std::mem::take(&mut review.comments), self.branch.as_deref());
            review.comments = here;
            others = elsewhere;
        }
        if let Some(local) = &self.local {
            let drafts = review::read(local)?.comments;
            // Yours only: posting moves these threads, and someone else's
            // would go out under your name.
            let roots: HashSet<String> = drafts
                .iter()
                .filter(|c| c.in_reply_to.is_none() && c.reference.is_none() && c.author.is_empty())
                .map(|c| c.id.clone())
                .collect();
            let drafts = drafts
                .into_iter()
                .filter(|c| {
                    c.reference.is_none() && roots.contains(c.in_reply_to.as_ref().unwrap_or(&c.id))
                })
                .collect();
            let (here, elsewhere) = by_branch(drafts, self.branch.as_deref());
            for c in here {
                from_local.insert(c.id.clone());
                review.comments.push(c);
            }
            others = elsewhere;
        }
        self.from_local = from_local;
        // Before the check: the local drafts may change when the review
        // doesn't.
        self.generation += 1;
        let others_changed = others != self.others;
        self.others = others;
        if review == self.review {
            return Ok(others_changed.then(Vec::new));
        }
        let mut by_path: HashMap<BString, Vec<usize>> = HashMap::new();
        for (i, comment) in review.comments.iter().enumerate() {
            by_path
                .entry(comment.path.as_bytes().into())
                .or_default()
                .push(i);
            // An old side's lines are the old file's, which a diff that
            // doesn't pair the rename lists on its own.
            if let (Some(old), Some((review::Side::Old, _))) = (&comment.old_path, comment.range())
                && *old != comment.path
            {
                by_path.entry(old.as_bytes().into()).or_default().push(i);
            }
        }
        let same = |path: &BString| {
            let old = self.by_path.get(path).map_or(&[][..], Vec::as_slice);
            let new = by_path.get(path).map_or(&[][..], Vec::as_slice);
            old.len() == new.len()
                && old
                    .iter()
                    .zip(new)
                    .all(|(&a, &b)| self.review.comments[a] == review.comments[b])
        };
        let mut changed: Vec<BString> = by_path
            .keys()
            .chain(self.by_path.keys().filter(|p| !by_path.contains_key(*p)))
            .filter(|p| !same(p))
            .cloned()
            .collect();
        changed.sort_unstable();
        for path in &changed {
            self.placed.remove(path);
        }
        self.review = review;
        self.by_path = by_path;
        self.conversation = OnceCell::new();
        self.general_len = self.general().count();
        Ok(Some(changed))
    }

    /// Applies `ops` to a file under its lock, then reads it back: the
    /// local review's when the first names a local draft by id (a reply
    /// in its thread, a change or a removal), or adds a thread while new
    /// threads go there, else the comments file. Returns the paths whose
    /// comments changed and the id each operation touched.
    pub(super) fn apply(
        &mut self,
        ops: Vec<Op>,
    ) -> Result<(Vec<BString>, Vec<Option<String>>), String> {
        let named = match ops.first() {
            Some(
                Op::Add {
                    reply_to: Some(review::Target::Id(id)),
                    ..
                }
                | Op::Set {
                    target: review::Target::Id(id),
                    ..
                }
                | Op::Rm(review::Target::Id(id)),
            ) => Some(id),
            _ => None,
        };
        let new_thread = matches!(ops.first(), Some(Op::Add { reply_to: None, .. }));
        let local = named.is_some_and(|id| {
            self.from_local.contains(id) || self.others.iter().any(|c| c.id == *id)
        }) || (new_thread && self.new_local);
        let file = match &self.local {
            Some(file) if local => file,
            _ => &self.path,
        };
        let ids = review::apply(file, ops)?;
        Ok((self.reload()?.unwrap_or_default(), ids))
    }

    /// New threads go to the local review's file rather than the
    /// comments file, when there's one.
    pub(super) fn set_new_local(&mut self, on: bool) {
        self.new_local = on && self.local.is_some();
    }

    /// Whether new threads written now go on the current branch: they go
    /// to the local review's file, which keeps branches apart.
    pub(super) fn branched(&self) -> bool {
        !self.session || self.new_local
    }

    /// The summary being written for the review, kept in the comments file.
    pub(super) fn summary(&self) -> Option<&str> {
        self.review.summary.as_deref()
    }

    /// Writes the review's summary to the comments file, unless it's what
    /// the file has already; empty removes it. Returns the paths whose
    /// comments changed in the read back, as another writer's may have.
    pub(super) fn keep_summary(&mut self, text: &str) -> Result<Vec<BString>, String> {
        let summary = Some(text.trim_end()).filter(|s| !s.is_empty());
        if summary == self.summary() {
            return Ok(Vec::new());
        }
        review::apply(&self.path, vec![Op::Summary(summary.map(str::to_owned))])?;
        Ok(self.reload()?.unwrap_or_default())
    }

    /// The path whose entry shows the thread with root `root` (see
    /// [`Comments::homes`]); empty for a general comment's.
    pub(super) fn home_of(&self, root: &str) -> Option<&str> {
        self.find(root).map(|c| self.home(c))
    }

    /// The posted threads' roots, by `ref`: what a run importing from the
    /// host is compared with, before and after, to find the new ones.
    pub(super) fn posted_roots(&self) -> HashMap<String, String> {
        self.review
            .comments
            .iter()
            .filter(|c| c.in_reply_to.is_none() && !c.path.is_empty())
            .filter_map(|c| Some((c.reference.clone()?, c.id.clone())))
            .collect()
    }

    /// How many of the comments shown are on the host for you only.
    pub(super) fn host_pending(&self) -> usize {
        if let Some((generation, pending)) = self.pending_counted.get()
            && generation == self.generation
        {
            return pending;
        }
        let pending = self.review.comments.iter().filter(|c| c.pending).count();
        self.pending_counted.set(Some((self.generation, pending)));
        pending
    }

    /// The drafts posting sends, in file order, local ones in view
    /// included: each one's id, whether it's from the local review's file,
    /// and whether the host refused it before.
    pub(super) fn drafts(&self) -> Vec<(&str, bool, bool)> {
        self.review
            .comments
            .iter()
            .filter(|c| self.standing(c) == Standing::Draft && !self.hides(c))
            .map(|c| {
                (
                    c.id.as_str(),
                    self.from_local.contains(&c.id),
                    c.post_error.is_some(),
                )
            })
            .collect()
    }

    /// How many local drafts in view were written before `epoch` (seconds
    /// since 1970), or at a time that doesn't read as one.
    pub(super) fn local_drafts_before(&self, epoch: u64) -> usize {
        self.local_shown()
            .filter(|c| self.standing(c) == Standing::Draft)
            .filter(|c| self::epoch(&c.created).is_none_or(|t| t < epoch))
            .count()
    }

    /// Takes what posting each draft does, by id. Returns the paths whose
    /// threads are to be placed again for it.
    pub(super) fn set_plans(&mut self, plans: HashMap<String, (Role, String)>) -> Vec<BString> {
        if plans == self.plans {
            return Vec::new();
        }
        let ids: HashSet<&String> = plans
            .iter()
            .filter(|&(id, word)| self.plans.get(id) != Some(word))
            .chain(self.plans.iter().filter(|(id, _)| !plans.contains_key(*id)))
            .map(|(id, _)| id)
            .collect();
        let mut paths: Vec<BString> = self
            .review
            .comments
            .iter()
            .filter(|c| ids.contains(&c.id))
            .map(|c| c.path.as_bytes().into())
            .collect();
        paths.sort_unstable();
        paths.dedup();
        for path in &paths {
            self.placed.remove(path);
        }
        self.plans = plans;
        self.generation += 1;
        self.conversation = OnceCell::new();
        paths
    }

    /// Whether `root` starts a thread on another branch.
    pub(super) fn elsewhere(&self, root: &str) -> bool {
        self.others.iter().any(|c| c.id == root)
    }

    /// Whether the host refused comment `id` when it was last posted.
    pub(super) fn refused(&self, id: &str) -> bool {
        self.find(id).is_some_and(|c| c.post_error.is_some())
    }

    /// `roots` in the order the threads are listed: by file, then line.
    pub(super) fn in_order(&self, roots: &HashSet<String>) -> Vec<String> {
        review::threads(
            self.review
                .comments
                .iter()
                .filter(|c| roots.contains(&c.id)),
        )
        .into_iter()
        .map(|c| c.id.clone())
        .collect()
    }

    /// The local drafts in view, replies included: what posting moves.
    fn local_shown(&self) -> impl Iterator<Item = &Comment> {
        self.review
            .comments
            .iter()
            .filter(|c| self.from_local.contains(&c.id) && !self.hides(c))
    }

    /// How many drafts [`Comments::move_local`] would move.
    pub(super) fn local_drafts(&self) -> usize {
        if self.from_local.is_empty() {
            return 0;
        }
        self.local_shown()
            .filter(|c| self.standing(c) == Standing::Draft)
            .count()
    }

    /// Moves the local drafts in view into the comments file, with new ids,
    /// and out of the local review's once that's written (see
    /// [`review::move_threads`]), then reads both again. Returns the paths
    /// whose comments changed.
    pub(super) fn move_local(&mut self) -> Result<Vec<BString>, String> {
        let roots: Vec<String> = self
            .local_shown()
            .filter(|c| c.in_reply_to.is_none())
            .map(|c| c.id.clone())
            .collect();
        let Some(local) = self.local.as_ref().filter(|_| !roots.is_empty()) else {
            return Ok(Vec::new());
        };
        review::move_threads(local, &self.path, &roots)?;
        Ok(self.reload()?.unwrap_or_default())
    }

    /// Finds the commented paths in the list `files`, after it changed
    /// order or content.
    pub(super) fn locate(&mut self, files: &[Entry]) {
        self.generation += 1;
        self.located.clear();
        self.renamed.clear();
        if self.by_path.is_empty() {
            return;
        }
        for (i, entry) in files.iter().enumerate() {
            self.note_entry(i, entry);
        }
    }

    /// Notes `entry`, just listed at `file`, if it has comments.
    pub(super) fn locate_one(&mut self, file: usize, entry: &Entry) {
        if self.note_entry(file, entry) {
            self.generation += 1;
        }
    }

    /// Notes where `entry` is when it has comments, and a rename's old
    /// path; returns whether it has.
    fn note_entry(&mut self, file: usize, entry: &Entry) -> bool {
        let change = entry.change();
        if entry.is_commit() || !self.by_path.contains_key(&change.path) {
            return false;
        }
        self.located.insert(change.path.clone(), file);
        if let Status::Renamed { from, .. } = &change.status {
            self.renamed.insert(change.path.clone(), from.clone());
        }
        true
    }

    /// Whether the thread starting with `root`, indexed under `key`, shows
    /// on that path's entry. An old side's comment shows on its file's
    /// entry when the diff pairs the rename, else on its old path's entry
    /// when the list has one.
    fn homes(&self, root: &Comment, key: &[u8]) -> bool {
        self.home(root).as_bytes() == key
    }

    /// The path whose entry shows the thread starting with `root` (see
    /// [`Comments::homes`]).
    fn home<'a>(&self, root: &'a Comment) -> &'a str {
        let Some(old) = root
            .old_path
            .as_deref()
            .filter(|_| matches!(root.range(), Some((review::Side::Old, _))))
        else {
            return &root.path;
        };
        let paired = self.renamed.get(root.path.as_bytes()).map(|f| &f[..]) == Some(old.as_bytes());
        if paired || !self.located.contains_key(old.as_bytes()) {
            &root.path
        } else {
            old
        }
    }

    pub(super) fn file_of(&self, path: &[u8]) -> Option<usize> {
        self.located.get(path).copied()
    }

    /// The files in the list with comments, in list order; not those whose
    /// only comments are local drafts that didn't place.
    pub(super) fn files(&self) -> Vec<usize> {
        let mut out: Vec<usize> = self
            .located
            .iter()
            .filter(|&(path, _)| self.placed.get(path).is_none_or(|p| !p.threads.is_empty()))
            .map(|(_, &file)| file)
            .collect();
        out.sort_unstable();
        out
    }

    /// Every path with comments, less those whose only comments are local
    /// drafts known not to place.
    pub(super) fn paths(&self) -> HashSet<BString> {
        self.by_path
            .keys()
            .filter(|&path| {
                !path.is_empty() && self.placed.get(path).is_none_or(|p| !p.threads.is_empty())
            })
            .cloned()
            .collect()
    }

    pub(super) fn has(&self, path: &[u8]) -> bool {
        self.by_path.contains_key(path)
    }

    /// The files whose threads aren't placed for their entry as it is now,
    /// in list order: their diffs are wanted.
    pub(super) fn pending(&self, files: &[Entry]) -> Vec<usize> {
        let mut out: Vec<usize> = self
            .located
            .iter()
            .filter(|&(path, &file)| {
                files.get(file).is_some_and(|e| {
                    self.placed
                        .get(path)
                        .is_none_or(|p| p.version != e.version())
                })
            })
            .map(|(_, &file)| file)
            .collect();
        out.sort_unstable();
        out
    }

    /// Places `path`'s threads on its old and new text, from version
    /// `version` of its entry; with no text (the diff failed), none find a
    /// line. `highlight` colors the code of those that don't.
    pub(super) fn place(
        &mut self,
        path: &[u8],
        version: u64,
        texts: Option<[&Text; 2]>,
        highlight: bool,
    ) {
        let Some(indices) = self.by_path.get(path) else {
            return;
        };
        let mut threads = Vec::new();
        let mut hidden = Vec::new();
        let roots = review::threads(indices.iter().map(|&i| &self.review.comments[i]));
        for comment in roots
            .into_iter()
            .filter(|c| c.in_reply_to.is_none() && self.homes(c, path))
        {
            let local = self.from_local.contains(&comment.id);
            let at = match comment.range() {
                None => At::File,
                Some((side, range)) => {
                    let found = texts.and_then(|texts| {
                        let text = texts[usize::from(side == review::Side::New)];
                        match &comment.anchor {
                            Some(anchor) => anchor.place(text, range.start),
                            None => (range.end <= text.len()).then_some(range),
                        }
                    });
                    if let Some(lines) = found {
                        let side = match side {
                            review::Side::Old => Side::Old,
                            review::Side::New => Side::New,
                        };
                        At::Line { side, lines }
                    } else if local {
                        hidden.push(comment.id.clone());
                        continue;
                    } else {
                        At::Lost
                    }
                }
            };
            threads.push(self.thread_of(comment, at, highlight));
        }
        self.generation += 1;
        self.placed.insert(
            path.into(),
            Placed {
                version,
                threads: threads.into(),
                hidden,
            },
        );
    }

    /// The thread whose first comment is `root`, shown `at`. `highlight`
    /// colors the code of one whose lines weren't found.
    fn thread_of(&self, root: &Comment, at: At, highlight: bool) -> Thread {
        self.thread_from(root, self.thread(&root.id), at, highlight)
    }

    /// The thread of `comments`, `root` first, shown `at`.
    fn thread_from(
        &self,
        root: &Comment,
        comments: Vec<&Comment>,
        at: At,
        highlight: bool,
    ) -> Thread {
        Thread {
            root: root.id.clone(),
            look: self.look(root),
            resolved: root.resolved,
            outdated: root.outdated,
            url: root.url.clone(),
            notes: comments
                .into_iter()
                .map(|c| Note {
                    id: c.id.clone(),
                    author: c.author.clone(),
                    created: c.created.clone(),
                    text: c.text.clone(),
                    summary: markdown::summary(&c.text),
                    suggestion: c.suggestion.clone(),
                    standing: self.standing(c),
                    pending: c.pending,
                    posting: self.posting(c),
                })
                .collect(),
            lines: root
                .anchor
                .as_ref()
                .map(|a| a.lines.clone())
                .unwrap_or_default(),
            snippet: (at == At::Lost).then(|| snippet(root, highlight)).flatten(),
            local: self.from_local.contains(&root.id),
            branch: None,
            at,
        }
    }

    /// What posting draft `c` does, when that's news: the host refused it
    /// before, else what the last plan said.
    fn posting(&self, c: &Comment) -> Option<(Role, String)> {
        if self.standing(c) != Standing::Draft {
            return None;
        }
        match &c.post_error {
            Some(why) => Some((Role::Warn, format!("refused: {}", escape(why.as_bytes())))),
            None => self.plans.get(&c.id).cloned(),
        }
    }

    /// The threads on other branches, for the comments panel: each with
    /// its branch, on its file.
    fn elsewhere_threads(&self) -> Vec<Thread> {
        self.others
            .iter()
            .filter(|c| c.in_reply_to.is_none())
            .map(|root| {
                let comments = std::iter::once(root)
                    .chain(
                        self.others
                            .iter()
                            .filter(|c| c.in_reply_to.as_ref() == Some(&root.id)),
                    )
                    .collect();
                let at = if root.range().is_some() {
                    At::Lost
                } else {
                    At::File
                };
                Thread {
                    branch: root.branch.as_deref().map(|b| escape(b.as_bytes())),
                    ..self.thread_from(root, comments, at, false)
                }
            })
            .collect()
    }

    /// The threads of `path`, once placed.
    pub(super) fn threads(&self, path: &[u8]) -> Option<Arc<[Thread]>> {
        self.placed.get(path).map(|p| Arc::clone(&p.threads))
    }

    /// The general comments that start a thread, in file order; none
    /// outside a review session, where nothing comes from a host.
    fn general(&self) -> impl Iterator<Item = &Comment> {
        self.by_path
            .get(&b""[..])
            .filter(|_| self.session)
            .into_iter()
            .flatten()
            .map(|&i| &self.review.comments[i])
            .filter(|c| c.in_reply_to.is_none())
    }

    /// How many general comments the conversation page shows, counted
    /// when the comments are read.
    pub(super) fn conversation_len(&self) -> usize {
        self.general_len
    }

    /// The conversation page's threads, one per general comment: the
    /// review's description first, then the rest by when they were written.
    /// Made on the first call after a read.
    pub(super) fn conversation(&self) -> Arc<[Thread]> {
        Arc::clone(self.conversation.get_or_init(|| {
            #[cfg(test)]
            CONVERSED.with(|n| n.set(n.get() + 1));
            let mut roots: Vec<&Comment> = self.general().collect();
            // Stable: comments of one time, or none, keep the file's order.
            roots.sort_by_key(|c| {
                (
                    c.kind.as_deref() != Some("description"),
                    epoch(&c.created).unwrap_or(u64::MAX),
                )
            });
            roots
                .into_iter()
                .map(|c| {
                    let at = At::Conversation {
                        kind: c.kind.clone(),
                        label: c.label.as_deref().map(|l| escape(l.as_bytes())),
                    };
                    self.thread_of(c, at, false)
                })
                .collect()
        }))
    }

    /// How many comments `path` has, and how the most pressing thread
    /// among them looks.
    pub(super) fn tally(&self, path: &[u8]) -> Option<(usize, Look)> {
        let mut tallied = self.tallied.borrow_mut();
        if tallied
            .as_ref()
            .is_none_or(|(generation, _)| *generation != self.generation)
        {
            let mut all = HashMap::new();
            for (path, comments) in &self.by_path {
                let mut shown = 0;
                let mut look = None;
                for c in comments.iter().map(|&i| &self.review.comments[i]) {
                    if self.hides(c) {
                        continue;
                    }
                    shown += 1;
                    if c.in_reply_to.is_none() {
                        look = look.max(Some(self.look(c)));
                    }
                }
                if shown > 0 {
                    all.insert(path.clone(), (shown, look.unwrap_or(Look::Open)));
                }
            }
            *tallied = Some((self.generation, all));
        }
        tallied.as_ref()?.1.get(path).copied()
    }

    /// Threads in the file shown and the local drafts in view, and the
    /// drafts among their comments. General comments aren't threads.
    pub(super) fn counts(&self) -> (usize, usize) {
        if let Some((generation, counts)) = self.counted.get()
            && generation == self.generation
        {
            return counts;
        }
        let shown = || self.review.comments.iter().filter(|c| !self.hides(c));
        let counts = (
            shown()
                .filter(|c| c.in_reply_to.is_none() && !c.path.is_empty())
                .count(),
            shown()
                .filter(|c| self.standing(c) == Standing::Draft)
                .count(),
        );
        self.counted.set(Some((self.generation, counts)));
        counts
    }

    fn look(&self, root: &Comment) -> Look {
        if root.resolved {
            return Look::Resolved;
        }
        let replies = self
            .by_path
            .get(root.path.as_bytes())
            .into_iter()
            .flatten()
            .map(|&i| &self.review.comments[i])
            .filter(|c| c.in_reply_to.as_deref() == Some(&root.id));
        if std::iter::once(root)
            .chain(replies)
            .any(|c| self.standing(c) == Standing::Draft)
        {
            Look::Draft
        } else {
            Look::Open
        }
    }

    fn find(&self, id: &str) -> Option<&Comment> {
        self.review.comments.iter().find(|c| c.id == id)
    }

    pub(super) fn thread_root(&self, id: &str) -> Option<String> {
        self.find(id)
            .map(|c| c.in_reply_to.clone().unwrap_or_else(|| id.to_owned()))
    }

    /// The thread whose root is `root`, root first, replies in file order;
    /// empty when it's gone.
    pub(super) fn thread(&self, root: &str) -> Vec<&Comment> {
        let Some(first) = self.find(root) else {
            return Vec::new();
        };
        let replies = self
            .by_path
            .get(first.path.as_bytes())
            .into_iter()
            .flatten()
            .map(|&i| &self.review.comments[i])
            .filter(|c| c.in_reply_to.as_deref() == Some(root));
        std::iter::once(first).chain(replies).collect()
    }

    /// Every thread's first comment, those on files not in the diff
    /// included, and how many commented files are still to be placed;
    /// `listing` while the list of files is still growing.
    fn listed(&self, listing: bool) -> (Vec<&Comment>, usize) {
        let mut out = Vec::new();
        let mut waiting: Vec<&str> = Vec::new();
        // General comments are the conversation's, listed apart.
        for root in review::threads(&self.review.comments)
            .into_iter()
            .filter(|c| c.in_reply_to.is_none() && !c.path.is_empty())
        {
            let path = root.path.as_bytes();
            let local = self.from_local.contains(&root.id);
            if local && self.located.contains_key(path) && !self.placed.contains_key(path) {
                waiting.push(&root.path);
            } else if self.hides(root) {
                // A local draft that isn't on this diff is the local
                // review's, not this one's.
            } else if root.range().is_none()
                || self.placed.contains_key(path)
                || (!self.located.contains_key(path) && !listing)
            {
                out.push(root);
            } else {
                waiting.push(&root.path);
            }
        }
        waiting.sort_unstable();
        waiting.dedup();
        (out, waiting.len())
    }

    /// [`Comments::listed`] by file: files in the list's order, then those
    /// not in it by path; a file's threads most pressing first, then by
    /// line. Placed threads are shared with the diff; threads on a file
    /// that isn't placed are made here, their code not highlighted.
    fn listing(&self, listing: bool) -> Listing {
        let (list, waiting) = self.listed(listing);
        let mut by_path: HashMap<&str, Vec<&Comment>> = HashMap::new();
        for &root in &list {
            by_path.entry(self.home(root)).or_default().push(root);
        }
        let mut paths: Vec<&str> = by_path.keys().copied().collect();
        paths.sort_unstable_by_key(|&p| (self.file_of(p.as_bytes()).unwrap_or(usize::MAX), p));
        let mut files = Vec::with_capacity(paths.len() + 1);
        let mut items = Vec::with_capacity(list.len());
        if self.conversation_len() > 0 {
            let threads = self.conversation();
            items.extend((0..threads.len()).map(|at| Item {
                group: 0,
                rank: Rank::of(&threads[at]),
                threads: Arc::clone(&threads),
                at,
                line: None,
            }));
            files.push(Group {
                path: String::new(),
                absent: false,
                conversation: true,
                branches: false,
            });
        }
        let general = items.len();
        for path in paths {
            let roots = &by_path[path];
            let absent = !self.located.contains_key(path.as_bytes());
            let threads: Arc<[Thread]> = match self.placed.get(path.as_bytes()) {
                Some(placed) => Arc::clone(&placed.threads),
                None => roots
                    .iter()
                    .map(|root| {
                        let at = if root.range().is_some() {
                            At::Lost
                        } else {
                            At::File
                        };
                        self.thread_of(root, at, false)
                    })
                    .collect(),
            };
            let start = items.len();
            for root in roots {
                let Some(at) = threads.iter().position(|t| t.root == root.id) else {
                    continue;
                };
                let line = match (&threads[at].at, root.range()) {
                    (At::Line { lines, .. }, _) => Some(lines.start + 1),
                    (_, Some((_, lines))) => Some(lines.start + 1),
                    (_, None) => None,
                };
                items.push(Item {
                    group: files.len(),
                    rank: Rank::of(&threads[at]),
                    threads: Arc::clone(&threads),
                    at,
                    line,
                });
            }
            items[start..].sort_by_key(|i| (i.rank, i.line.unwrap_or(0)));
            files.push(Group {
                path: path.to_owned(),
                absent,
                conversation: false,
                branches: false,
            });
        }
        #[cfg(test)]
        LISTED.with(|n| n.set(n.get() + 1));
        let threads = &items[general..];
        let counts = [
            threads.len(),
            threads.iter().filter(|i| !i.thread().resolved).count(),
        ];
        let elsewhere: Arc<[Thread]> = self.elsewhere_threads().into();
        if !elsewhere.is_empty() {
            items.extend((0..elsewhere.len()).map(|at| Item {
                group: files.len(),
                rank: Rank::of(&elsewhere[at]),
                threads: Arc::clone(&elsewhere),
                at,
                line: None,
            }));
            files.push(Group {
                path: String::new(),
                absent: true,
                conversation: false,
                branches: true,
            });
        }
        Listing {
            key: (self.generation, listing),
            counts,
            files,
            items,
            waiting,
        }
    }
}

/// `comments` split into the threads that show on `branch`, `None` for a
/// detached HEAD, and the rest, each judged by its first comment.
fn by_branch(comments: Vec<Comment>, branch: Option<&str>) -> (Vec<Comment>, Vec<Comment>) {
    let elsewhere: HashSet<String> = comments
        .iter()
        .filter(|c| c.in_reply_to.is_none() && !c.on_branch(branch))
        .map(|c| c.id.clone())
        .collect();
    if elsewhere.is_empty() {
        return (comments, Vec::new());
    }
    comments
        .into_iter()
        .partition(|c| !elsewhere.contains(c.in_reply_to.as_ref().unwrap_or(&c.id)))
}

/// The line diff of a loaded file, the source of a rendered view: what
/// comments are anchored in.
pub(super) fn line_diff(file: &File) -> &Arc<FileDiff> {
    file.rendered().map_or_else(|| file.diff(), |r| r.source())
}

/// A new comment, without its text, on `lines` (0-based, non-empty) of
/// `side` of `diff`, the line diff of `change`.
pub(super) fn draft(
    change: &FileChange,
    diff: &FileDiff,
    side: Side,
    lines: Range<usize>,
) -> Result<Comment, &'static str> {
    let (text, ours) = match side {
        Side::Old => (&diff.old, review::Side::Old),
        Side::New => (&diff.new, review::Side::New),
    };
    if lines.is_empty() || lines.end > text.len() {
        return Err("no line of code here to comment on");
    }
    Ok(Comment {
        side: ours,
        start: Some(numbers(&diff.hunks, side, lines.start)),
        end: Some(numbers(&diff.hunks, side, lines.end - 1)),
        anchor: Some(Anchor::new(text, lines.clone())),
        distance: review::distance(lines, ours, &diff.hunks),
        ..file_draft(change)?
    })
}

/// A new comment, without its text, on the whole file of `change`.
pub(super) fn file_draft(change: &FileChange) -> Result<Comment, &'static str> {
    let utf8 = |path: &[u8]| {
        std::str::from_utf8(path)
            .map(str::to_owned)
            .map_err(|_| "the comments file holds UTF-8 paths only; this one isn't")
    };
    let old_path = match &change.status {
        Status::Renamed { from, .. } => Some(utf8(from)?),
        _ => None,
    };
    Ok(Comment {
        path: utf8(&change.path)?,
        old_path,
        ..Comment::default()
    })
}

/// Line `line` of `side`, with its number on the other side when it's on
/// both: from the hunk holding it, else shifted as the hunks above shift
/// lines.
fn numbers(hunks: &[Hunk], side: Side, line: usize) -> LineNumbers {
    let kind = match side {
        Side::Old => LineKind::Removed,
        Side::New => LineKind::Added,
    };
    let mine = |l: &dffy_core::diff::Line| match side {
        Side::Old => l.old,
        Side::New => l.new,
    };
    let theirs = |l: &dffy_core::diff::Line| match side {
        Side::Old => l.new,
        Side::New => l.old,
    };
    let mut other = Some(line);
    for hunk in hunks {
        let ((start, len), (other_start, other_len)) = match side {
            Side::Old => (
                (hunk.old_start, hunk.old_len),
                (hunk.new_start, hunk.new_len),
            ),
            Side::New => (
                (hunk.new_start, hunk.new_len),
                (hunk.old_start, hunk.old_len),
            ),
        };
        if line < start {
            break;
        }
        if line < start + len {
            other = hunk
                .lines
                .iter()
                .find(|l| mine(l) == line && (l.kind == LineKind::Context || l.kind == kind))
                .filter(|l| l.kind == LineKind::Context)
                .map(theirs);
            break;
        }
        other = Some(line - (start + len) + other_start + other_len);
    }
    match side {
        Side::Old => LineNumbers {
            old: Some(line),
            new: other,
        },
        Side::New => LineNumbers {
            old: other,
            new: Some(line),
        },
    }
}

/// What a thread whose lines weren't found shows of them, from its first
/// comment; `None` for a comment on the whole file. `highlight` colors the
/// commented lines in the file's language, parsed with their context.
///
/// A line's sign is its side's (`+` new, `-` old) unless the comment says
/// it was unchanged: dffy numbers such a line on both sides. Hosts number
/// one side only, so their threads always show the side's sign, and lines
/// between a range's first and last take it unless both ends were
/// unchanged.
pub(crate) fn snippet(comment: &Comment, highlight: bool) -> Option<Snippet> {
    let (side, range) = comment.range()?;
    let (first, last) = (range.start + 1, range.end);
    let old = if side == review::Side::Old {
        "old "
    } else {
        ""
    };
    let heading = if first == last {
        format!("{old}line {first} not found")
    } else {
        format!("{old}lines {first}–{last} not found")
    };
    let Some(anchor) = &comment.anchor else {
        return Some(Snippet {
            heading,
            rows: Vec::new(),
        });
    };
    let lines: Vec<&str> = anchor
        .before
        .iter()
        .chain(&anchor.lines)
        .chain(&anchor.after)
        .map(String::as_str)
        .collect();
    let path = match (side, &comment.old_path) {
        (review::Side::Old, Some(old)) => old,
        _ => &comment.path,
    };
    let spans = highlight.then(|| painted(path, &lines)).flatten();
    let unchanged = |n: Option<LineNumbers>| n.is_some_and(|n| n.old.is_some() && n.new.is_some());
    let (start, end) = (unchanged(comment.start), unchanged(comment.end));
    let (before, n) = (anchor.before.len(), anchor.lines.len());
    let rows = lines
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let number = (range.start + 1 + i).saturating_sub(before);
            let Some(k) = i.checked_sub(before).filter(|&k| k < n) else {
                return CodeRow {
                    number,
                    sign: " ",
                    pieces: vec![(Role::Dim.into(), plain(text))],
                };
            };
            let kept = match (k == 0, k + 1 == n) {
                (_, true) => end,
                (true, false) => start,
                (false, false) => start && end,
            };
            CodeRow {
                number,
                sign: match (kept, side) {
                    (true, _) => " ",
                    (false, review::Side::Old) => "-",
                    (false, review::Side::New) => "+",
                },
                pieces: pieces(text, spans.as_ref().map(|s| s.line(i))),
            }
        })
        .collect();
    Some(Snippet { heading, rows })
}

#[cfg(test)]
thread_local! {
    static PAINTED: Cell<usize> = const { Cell::new(0) };
}

/// Times [`painted`] ran on this thread, to tell that drawing never does.
#[cfg(test)]
pub(crate) fn painted_count() -> usize {
    PAINTED.with(Cell::get)
}

#[cfg(test)]
thread_local! {
    static LISTED: Cell<usize> = const { Cell::new(0) };
}

/// Times the comments panel's list was made on this thread, to tell that
/// it isn't while the panel shows the files.
#[cfg(test)]
pub(crate) fn listed_count() -> usize {
    LISTED.with(Cell::get)
}

#[cfg(test)]
thread_local! {
    static CONVERSED: Cell<usize> = const { Cell::new(0) };
}

/// Times the conversation's threads were made on this thread, to tell that
/// they aren't until something shows them.
#[cfg(test)]
pub(crate) fn conversed_count() -> usize {
    CONVERSED.with(Cell::get)
}

/// The syntax runs of `lines`, parsed together as `path`'s language would
/// be; `None` without one, or when they don't parse in time.
fn painted(path: &str, lines: &[&str]) -> Option<Spans> {
    #[cfg(test)]
    PAINTED.with(|n| n.set(n.get() + 1));
    let mut bytes = Vec::new();
    for line in lines {
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
    }
    let lang = crate::grammars().detect(path.as_bytes(), &bytes[..bytes.len().min(256)])?;
    let text = Text::new(bytes);
    Some(highlight::parse(lang, text.bytes())?.spans(&text, 0..text.len()))
}

/// A line of code to draw in a box: tabs as four spaces, escaped.
pub(super) fn plain(text: &str) -> String {
    escape(text.replace('\t', "    ").as_bytes())
}

/// `text` split at `runs`' edges, each piece in its syntax's ink; plain
/// without runs.
pub(super) fn pieces(text: &str, runs: Option<&[Run]>) -> Vec<(Ink, String)> {
    let Some(runs) = runs else {
        return vec![(Role::Plain.into(), plain(text))];
    };
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut push = |ink: Ink, range: Range<usize>| {
        if !range.is_empty() {
            out.push((ink, plain(&String::from_utf8_lossy(&bytes[range]))));
        }
    };
    let mut at = 0;
    for run in runs {
        let (start, end) = (run.start as usize, (run.end as usize).min(bytes.len()));
        if start < at || start >= end {
            continue;
        }
        push(Role::Plain.into(), at..start);
        let ink = Ink {
            syntax: Some(run.syntax),
            ..Role::Plain.into()
        };
        push(ink, start..end);
        at = end;
    }
    push(Role::Plain.into(), at..bytes.len());
    out
}

/// `thread` as text to paste: each comment as written, under a line of its
/// author and the day it was written, a blank line apart.
pub(crate) fn thread_text(thread: &Thread) -> String {
    let comments: Vec<String> = thread
        .notes
        .iter()
        .map(|note| {
            let day = note.created.get(..10).unwrap_or(&note.created);
            let text = note.text.trim_end_matches(['\n', '\r']);
            format!("{} · {day}\n{text}", who(&note.author))
        })
        .collect();
    comments.join("\n\n")
}

/// Seconds since 1970 at `created`, an RFC 3339 time; `None` when it isn't
/// one.
fn epoch(created: &str) -> Option<u64> {
    let num = |r: Range<usize>| created.get(r)?.parse::<u64>().ok();
    let b = created.as_bytes();
    if (b.get(4), b.get(7), b.get(13), b.get(16))
        != (Some(&b'-'), Some(&b'-'), Some(&b':'), Some(&b':'))
    {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let secs = num(11..13)? * 3600 + num(14..16)? * 60 + num(17..19)?;
    let rest = created.get(19..)?;
    let rest = rest
        .strip_prefix('.')
        .map_or(rest, |r| r.trim_start_matches(|c: char| c.is_ascii_digit()));
    let offset = match rest.as_bytes() {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), ..] if rest.len() == 6 => {
            let n = |r: Range<usize>| rest.get(r)?.parse::<i64>().ok();
            let minutes = n(1..3)? * 60 + n(4..6)?;
            if *sign == b'+' { minutes } else { -minutes }
        }
        _ => return None,
    };
    // Howard Hinnant's days_from_civil, for years from 1970.
    let y = if month <= 2 {
        year.checked_sub(1)?
    } else {
        year
    };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day.checked_sub(1)?;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = (era * 146_097 + doe).checked_sub(719_468)?;
    let utc = i64::try_from(days * 86_400 + secs).ok()? - offset * 60;
    u64::try_from(utc).ok()
}

/// How long ago `created` was at `now` (seconds since 1970): `just now`,
/// `5m ago` up to `29d ago`, then its date.
pub(crate) fn age(created: &str, now: u64) -> String {
    let date = || escape(created.get(..10).unwrap_or(created).as_bytes());
    let Some(then) = epoch(created) else {
        return date();
    };
    match now.saturating_sub(then) {
        s if s < 60 => "just now".to_owned(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 30 * 86_400 => format!("{}d ago", s / 86_400),
        _ => date(),
    }
}

const AUTHOR: Ink = Ink {
    role: Role::Plain,
    syntax: None,
    attributes: Attributes::BOLD,
};

const DIM: Ink = Ink {
    role: Role::Dim,
    syntax: None,
    attributes: Attributes::NONE,
};

fn spans_width<I>(spans: &[(I, &str)]) -> usize {
    spans.iter().map(|(_, t)| t.width()).sum()
}

/// Pushes `spans`, cut to `width` columns with `…` when they're longer.
/// Returns the columns they took.
fn cut<I: Into<Ink> + Copy>(c: &mut Canvas, spans: &[(I, &str)], width: usize) -> usize {
    let total = spans_width(spans);
    if total <= width {
        for (ink, text) in spans {
            c.push_ink((*ink).into(), text);
        }
        return total;
    }
    if width == 0 {
        return 0;
    }
    let mut used = 0;
    for (ink, text) in spans {
        let room = width - 1 - used;
        if text.width() <= room {
            c.push_ink((*ink).into(), text);
            used += text.width();
            continue;
        }
        let mut cut = String::new();
        for ch in text.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > width - 1 {
                break;
            }
            used += w;
            cut.push(ch);
        }
        c.push_ink((*ink).into(), &cut);
        break;
    }
    c.push(Role::Dim, "…");
    used + 1
}

/// Pushes `width` columns: `left` cut to fit, and `right` at the right end
/// when that leaves a dozen columns of `left`.
fn spread(c: &mut Canvas, left: &[(Ink, &str)], right: &[(Role, &str)], width: usize) {
    let right_width = match spans_width(right) {
        w if w > 0 && w + 2 + 12 <= width => w,
        _ => 0,
    };
    let room = match right_width {
        0 => width,
        w => width - w - 2,
    };
    let used = cut(c, left, room);
    c.spaces(Role::Plain, width - used - right_width);
    if right_width > 0 {
        cut(c, right, right_width);
    }
}

/// The lines of a thread's box: light, heavy while it has the focus, and
/// with round corners while it holds a draft. The corners tell an open
/// draft from other threads without color, as the theme is attributes
/// only; focused, or folded to a card without corners, only the role
/// does. Lines stay solid: dashed ones break up into dots in some fonts.
#[derive(Clone, Copy)]
struct Border {
    role: Role,
    /// Top left, top right, bottom left, bottom right.
    corners: [&'static str; 4],
    across: &'static str,
    down: &'static str,
}

impl Border {
    fn new(draft: bool, focused: bool) -> Self {
        let role = match (draft, focused) {
            (true, _) => Role::Warn,
            (false, true) => Role::Hint,
            (false, false) => Role::Dim,
        };
        let (across, down) = if focused {
            ("━", "┃")
        } else {
            ("─", "│")
        };
        let corners = match (draft, focused) {
            (_, true) => ["┏", "┓", "┗", "┛"],
            (true, false) => ["╭", "╮", "╰", "╯"],
            (false, false) => ["┌", "┐", "└", "┘"],
        };
        Self {
            role,
            corners,
            across,
            down,
        }
    }
}

/// Writes a box's rows into a canvas, each exactly `width` columns, with a
/// cell of padding inside the sides.
struct Frame<'a> {
    c: &'a mut Canvas,
    border: Border,
    width: usize,
}

/// Columns of text inside the padding of a box `width` columns wide.
pub(crate) fn inner(width: usize) -> usize {
    width.saturating_sub(4).max(1)
}

/// Columns from a box's left edge to its text: the side and its padding.
pub(crate) const TEXT_COLUMN: usize = 2;

/// The text of each of `thread`'s comments rendered for its box, `width`
/// columns wide: what [`thread_rows`] draws open. `highlight` colors their
/// fenced code.
pub(crate) fn bodies(thread: &Thread, width: usize, highlight: bool) -> Vec<Body> {
    thread
        .notes
        .iter()
        .map(|note| markdown::render(&note.text, inner(width), highlight))
        .collect()
}

impl Frame<'_> {
    fn inner(&self) -> usize {
        inner(self.width)
    }

    /// A row of the border between `ends`, carrying `spans` after the
    /// first when it has room for them.
    fn edge(&mut self, ends: [&str; 2], spans: &[(Ink, &str)]) {
        let Border { role, across, .. } = self.border;
        let room = self.width.saturating_sub(2);
        self.c.push(role, ends[0]);
        let mut used = 0;
        if !spans.is_empty() && room >= 3 {
            self.c.push(Role::Plain, " ");
            used += 2 + cut(self.c, spans, room - 2);
            self.c.push(Role::Plain, " ");
        }
        self.c.push(role, &across.repeat(room.saturating_sub(used)));
        self.c.push(role, ends[1]);
        self.c.end_line();
    }

    /// A folded thread's one row, between the box's sides.
    fn card(&mut self, left: &[(Ink, &str)], right: &[(Role, &str)]) {
        let Border { role, down, .. } = self.border;
        let inner = self.inner();
        self.c.push(role, down);
        self.c.push(Role::Plain, " ");
        spread(self.c, left, right, inner);
        self.c.push(Role::Plain, " ");
        self.c.push(role, down);
        self.c.end_line();
    }

    fn top(&mut self, spans: &[(Ink, &str)]) {
        let [left, right, ..] = self.border.corners;
        self.edge([left, right], spans);
    }

    /// A reply's heading, under a blank row parting it from the comment
    /// above.
    fn heading(&mut self, spans: &[(Ink, &str)]) {
        self.body::<Role>(&[]);
        self.body(spans);
    }

    fn bottom(&mut self) {
        let [.., left, right] = self.border.corners;
        self.edge([left, right], &[]);
    }

    /// A row of text inside the box, cut to fit.
    fn body<I: Into<Ink> + Copy>(&mut self, spans: &[(I, &str)]) {
        let Border { role, down, .. } = self.border;
        let inner = self.inner();
        self.c.push(role, down);
        self.c.push(Role::Plain, " ");
        let used = cut(self.c, spans, inner);
        self.c.spaces(Role::Plain, inner - used + 1);
        self.c.push(role, down);
        self.c.end_line();
    }

    /// A snippet's code, numbered, each line cut to fit as a suggestion's
    /// are, then a blank row parting it from the comment.
    fn code(&mut self, snippet: &Snippet) {
        let Some(last) = snippet.rows.iter().map(|r| r.number).max() else {
            return;
        };
        let digits = last.to_string().len();
        for row in &snippet.rows {
            let number = format!("{:>digits$} ", row.number);
            let sign = match row.sign {
                "+" => Role::Added,
                "-" => Role::Removed,
                _ => Role::Dim,
            };
            let mut spans: Vec<(Ink, &str)> = vec![
                (Role::Dim.into(), &number),
                (sign.into(), row.sign),
                (Role::Plain.into(), " "),
            ];
            spans.extend(row.pieces.iter().map(|(ink, text)| (*ink, text.as_str())));
            self.body(&spans);
        }
        self.body::<Role>(&[]);
    }

    /// The text box's wrapped rows, its cursor a bar in the text.
    fn text_box(&mut self, text: &mut TextBox) {
        // A column for the cursor.
        let room = self.inner().saturating_sub(1).max(1);
        for (line, cursor) in text.rows(room) {
            match cursor {
                Some(at) => self.body(&[
                    (Role::Plain, &line[..at]),
                    (Role::Hint, "▏"),
                    (Role::Plain, &line[at..]),
                ]),
                None => self.body(&[(Role::Plain, &line)]),
            }
        }
    }
}

/// What a comment's heading says of where it stands, when that's news: a
/// comment shows posted by its link, and outside a review session nothing
/// is posted.
fn standing_tag(standing: Standing) -> Option<(Role, &'static str)> {
    match standing {
        Standing::Draft => Some((Role::Warn, "draft")),
        Standing::Unposted => Some((Role::Dim, "not posted")),
        Standing::Posted | Standing::Kept => None,
    }
}

/// The words after a comment's author: where it stands, what posting it
/// does, and whether it's on the host for you only.
fn note_tags(note: &Note) -> impl Iterator<Item = (Role, &str)> {
    standing_tag(note.standing)
        .into_iter()
        .chain(
            note.posting
                .as_ref()
                .map(|(role, word)| (*role, word.as_str())),
        )
        .chain(note.pending.then_some((Role::Hint, "pending")))
}

/// The words after a thread's first author: its state, then where it is
/// when that's not on a line, with the line it was on.
fn tags<'a>(thread: &'a Thread, first: &'a Note) -> Vec<(Role, &'a str)> {
    let mut out: Vec<(Role, &str)> = note_tags(first).collect();
    if thread.local {
        out.push((Role::Hint, "local"));
    }
    if thread.resolved {
        out.push((Role::Dim, "resolved"));
    }
    if thread.outdated {
        out.push((Role::Warn, "outdated"));
    }
    match (&thread.at, &thread.snippet) {
        (At::File, _) => out.push((Role::Dim, "on the file")),
        (At::Lost, Some(snippet)) => out.push((Role::Warn, &snippet.heading)),
        (At::Lost, _) => out.push((Role::Warn, "lines not found")),
        (At::Conversation { kind, label }, _) => {
            out.extend(shown_kind(kind.as_deref()).map(|k| (Role::Dim, k)));
            out.extend(label.as_deref().map(|l| (Role::Hint, l)));
        }
        (At::Line { .. }, _) => {}
    }
    out
}

/// The word a general comment's `kind` shows as; none for a plain comment
/// and for a kind dffy doesn't know, which shows as one.
fn shown_kind(kind: Option<&str>) -> Option<&'static str> {
    match kind? {
        "description" => Some("description"),
        "review" => Some("review"),
        _ => None,
    }
}

/// Where a comment is in its thread's open box, in rows from the box's top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoteRows {
    /// Its heading's: the top border for the first comment.
    pub first: usize,
    /// Its rendered text's; none while the text box edits it.
    pub text: Range<usize>,
}

/// Writes `thread`'s box, `width` columns wide. Collapsed, one row between
/// the box's sides sums it up, and it returns true: inks carry no
/// background, so the caller tints that row. Open, it draws `bodies`, its
/// [`bodies`] at `width`.
/// `focused` draws the border heavy. `writing`, when the text box is in
/// this thread, takes the place of the comment it edits or follows the
/// last as a reply. `now` is seconds since 1970. Open, `notes` gets where
/// each comment is in the box.
pub(crate) fn thread_rows(
    thread: &Thread,
    bodies: &[Body],
    (collapsed, focused): (bool, bool),
    mut writing: Option<&mut Writing>,
    (width, now): (usize, u64),
    (c, notes): (&mut Canvas, &mut Vec<NoteRows>),
) -> bool {
    notes.clear();
    let Some(first) = thread.notes.first() else {
        return false;
    };
    let border = Border::new(thread.look == Look::Draft, focused);
    let top = c.len();
    let mut frame = Frame { c, border, width };
    if collapsed && writing.is_none() {
        let author = who(&first.author);
        let when = format!(" · {}", age(&first.created, now));
        let replies = match thread.notes.len() - 1 {
            0 => String::new(),
            1 => "1 reply".to_owned(),
            n => format!("{n} replies"),
        };
        let mut spans = vec![
            (Role::Dim.into(), "▸ "),
            (AUTHOR, author.as_str()),
            (Role::Dim.into(), when.as_str()),
        ];
        for (role, tag) in tags(thread, first) {
            spans.extend([(Role::Dim.into(), " · "), (role.into(), tag)]);
        }
        spans.extend([
            (Role::Dim.into(), " · "),
            (Role::Plain.into(), first.summary.as_str()),
        ]);
        frame.card(&spans, &[(Role::Dim, &replies)]);
        return true;
    }
    for (i, note) in thread.notes.iter().enumerate() {
        let author = who(&note.author);
        let when = format!(" · {}", age(&note.created, now));
        let mut spans = if i == 0 {
            vec![(Role::Dim.into(), "▾ ")]
        } else {
            Vec::new()
        };
        spans.extend([(AUTHOR, author.as_str()), (Role::Dim.into(), when.as_str())]);
        let words = if i == 0 {
            tags(thread, note)
        } else {
            note_tags(note).collect()
        };
        for (role, word) in words {
            spans.extend([(Role::Dim.into(), " · "), (role.into(), word)]);
        }
        let editing = writing
            .as_deref_mut()
            .filter(|w| matches!(&w.at, Writes::Edit { id, .. } if *id == note.id));
        if editing.is_some() {
            spans.push((Role::Dim.into(), " · esc saves · ctrl+g editor"));
        }
        // A reply's heading is under a blank row.
        let heading = frame.c.len() - top + usize::from(i > 0);
        if i == 0 {
            frame.top(&spans);
            if let Some(snippet) = &thread.snippet {
                frame.code(snippet);
            }
        } else {
            frame.heading(&spans);
        }
        let from = frame.c.len() - top;
        if let Some(writing) = editing {
            frame.text_box(&mut writing.text);
            notes.push(NoteRows {
                first: heading,
                text: from..from,
            });
            continue;
        }
        for row in bodies.get(i).iter().flat_map(|body| body.rows.rows()) {
            frame.body(&row.collect::<Vec<_>>());
        }
        notes.push(NoteRows {
            first: heading,
            text: from..frame.c.len() - top,
        });
        if let Some(suggestion) = &note.suggestion {
            frame.body(&[(Role::Dim, "suggested change:")]);
            let old = thread
                .lines
                .iter()
                .map(|l| (Role::Removed, "- ", l.as_str()));
            let new = suggestion.lines().map(|l| (Role::Added, "+ ", l));
            for (role, sign, text) in old.chain(new) {
                frame.body(&[(role, sign), (role, &plain(text))]);
            }
        }
    }
    if let Some(writing) = writing.filter(|w| matches!(w.at, Writes::Reply(_))) {
        frame.heading(&[
            (AUTHOR, "you"),
            (Role::Dim.into(), " · reply · esc saves · ctrl+g editor"),
        ]);
        frame.text_box(&mut writing.text);
    }
    frame.bottom();
    false
}

/// Writes the text box for a new thread, `width` columns wide: a draft's
/// box with the focus, its heading in the top border.
pub(crate) fn new_rows(text: &mut TextBox, width: usize, c: &mut Canvas) {
    let mut frame = Frame {
        c,
        border: Border::new(true, true),
        width,
    };
    frame.top(&[
        (Role::Dim.into(), "▾ "),
        (AUTHOR, "you"),
        (
            Role::Dim.into(),
            " · new comment · esc saves, empty discards · ctrl+g editor",
        ),
    ]);
    frame.text_box(text);
    frame.bottom();
}

/// What a key in the comments panel did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Stay,
    /// The selection moved to the thread with this root: the diff shows it.
    Moved(String),
    /// The keys go to the diff.
    Diff,
    /// Not the panel's: the key does what it does in the tree.
    Pass,
}

/// The filters in the order `f` steps through them, as the title names
/// them, and what each says when it keeps nothing.
const FILTERS: [(&str, &str); 4] = [
    ("", "no threads"),
    ("open", "no open threads"),
    ("drafts", "no drafts"),
    ("resolved", "no resolved threads"),
];
/// A thread's row narrower than this leaves out its line, and narrower
/// than [`REPLIES_COLUMNS`] its replies too.
const LINE_COLUMNS: usize = 23;
const REPLIES_COLUMNS: usize = 19;

/// How pressing a thread is, most first: the order of a file's threads in
/// the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Draft,
    Open,
    Outdated,
    Resolved,
}

impl Rank {
    /// A thread with a draft in it is a draft, even resolved: posting
    /// sends it.
    fn of(thread: &Thread) -> Self {
        if thread.notes.iter().any(|n| n.standing == Standing::Draft) {
            Self::Draft
        } else if thread.resolved {
            Self::Resolved
        } else if thread.outdated {
            Self::Outdated
        } else {
            Self::Open
        }
    }
}

/// Every thread the panel lists, by file, made from one generation of the
/// comments.
struct Listing {
    /// That generation, and whether the list of files was still growing.
    key: (u64, bool),
    files: Vec<Group>,
    /// File by file, in [`Listing::files`]' order.
    items: Vec<Item>,
    /// Threads, and open ones; general comments aren't threads.
    counts: [usize; 2],
    /// Commented files still to be placed.
    waiting: usize,
}

struct Group {
    path: String,
    /// Not in the list of files.
    absent: bool,
    /// The general comments, listed first: not a file's.
    conversation: bool,
    /// The threads on other branches, listed last while asked for.
    branches: bool,
}

struct Item {
    /// Its file, in [`Listing::files`].
    group: usize,
    rank: Rank,
    /// Its file's threads, and which it is.
    threads: Arc<[Thread]>,
    at: usize,
    /// Its first line, 1-based; `None` on the whole file.
    line: Option<usize>,
}

impl Item {
    fn thread(&self) -> &Thread {
        &self.threads[self.at]
    }
}

/// A row of the panel's list.
enum Row {
    /// A file's header, with how many of its threads are kept.
    File {
        group: usize,
        threads: usize,
    },
    /// A thread, by its index in [`Listing::items`]; `author` when the row
    /// names its author, who isn't the row above's.
    Thread {
        item: usize,
        author: bool,
    },
    Note(String),
}

/// The comment threads listed by file in the left panel, in place of the
/// tree (`i`). Moving the selection shows the thread in the diff, which is
/// its preview: see [`Outcome::Moved`].
///
/// The list is made when the panel opens, and again only when the
/// comments or the list of files change; its rows when the filter or the
/// search change too. A frame draws the rows on screen.
pub(super) struct Panel {
    /// Which of [`FILTERS`].
    filter: usize,
    /// Typed after `/`, lowercase.
    search: String,
    /// Keys go to the search.
    pub editing: bool,
    /// The threads on other branches are listed (`b`); folded to one row
    /// otherwise.
    others: bool,
    listing: Option<Listing>,
    rows: Vec<Row>,
    /// The filter, the search or `others` changed since `rows` were made.
    stale: bool,
    /// The last root [`Panel::select`] didn't find in `rows`, so a frame
    /// with a thread focused that the filter hides doesn't look again.
    missed: Option<String>,
    /// A thread's row, when there's one.
    selected: usize,
    scroll: usize,
    /// Rows the panel showed last, for paging and taps.
    height: usize,
}

impl Panel {
    /// `listing` while the list of files is still growing. The selection
    /// starts on the thread with root `near.0`, else on the first of file
    /// `near.1`'s, else on the first.
    pub(super) fn new(comments: &Comments, listing: bool, near: (Option<&str>, &[u8])) -> Self {
        let mut panel = Self {
            filter: 0,
            search: String::new(),
            editing: false,
            others: false,
            listing: None,
            rows: Vec::new(),
            stale: true,
            missed: None,
            selected: 0,
            scroll: 0,
            height: 1,
        };
        panel.sync(comments, listing);
        if let Some(list) = &panel.listing
            && let Some(i) = panel.rows.iter().position(|r| match r {
                Row::Thread { item, .. } => {
                    let item = &list.items[*item];
                    near.0.map_or_else(
                        || list.files[item.group].path.as_bytes() == near.1,
                        |root| item.thread().root == root,
                    )
                }
                _ => false,
            })
        {
            panel.selected = i;
        }
        panel
    }

    /// The selected thread's root.
    pub(super) fn selected(&self) -> Option<&str> {
        self.thread().map(|t| t.root.as_str())
    }

    /// The selected thread, as the list was last made: placed, or made
    /// for the panel when its file isn't in the diff.
    pub(super) fn thread(&self) -> Option<&Thread> {
        let Row::Thread { item, .. } = self.rows.get(self.selected)? else {
            return None;
        };
        Some(self.listing.as_ref()?.items[*item].thread())
    }

    pub(super) fn select(&mut self, root: &str) {
        if self.selected() == Some(root) || self.missed.as_deref() == Some(root) {
            return;
        }
        let Some(list) = &self.listing else {
            return;
        };
        match self.rows.iter().position(
            |r| matches!(r, Row::Thread { item, .. } if list.items[*item].thread().root == root),
        ) {
            Some(i) => self.selected = i,
            None => self.missed = Some(root.to_owned()),
        }
    }

    /// The wheel moves the selection a thread a notch. Returns the root it
    /// moved to.
    pub(super) fn scroll_by(&mut self, by: isize) -> Option<String> {
        let before = self.selected().map(str::to_owned);
        self.step(by);
        self.selected()
            .filter(|&r| before.as_deref() != Some(r))
            .map(str::to_owned)
    }

    /// Selects the thread at panel line `y` (0 is the first row below the
    /// border), and returns its root.
    pub(super) fn click(&mut self, y: usize) -> Option<String> {
        let row = self.scroll + y;
        if y >= self.height || !matches!(self.rows.get(row), Some(Row::Thread { .. })) {
            return None;
        }
        self.selected = row;
        self.selected().map(str::to_owned)
    }

    /// Moves the selection `by` threads, over the headers, stopping at
    /// either end.
    fn step(&mut self, by: isize) {
        let threads: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, Row::Thread { .. }))
            .map(|(i, _)| i)
            .collect();
        if let Some(at) = threads.iter().position(|&i| i == self.selected) {
            self.selected = threads[at.saturating_add_signed(by).min(threads.len() - 1)];
        }
    }

    /// Makes the list again when the comments or the list of files
    /// changed, and the rows when the filter or the search did too,
    /// keeping the selected thread when it's still listed, else the next
    /// one.
    fn sync(&mut self, comments: &Comments, listing: bool) {
        let key = (comments.generation, listing);
        let old = self.listing.as_ref().is_none_or(|l| l.key != key);
        if !old && !self.stale {
            return;
        }
        let root = self.selected().map(str::to_owned);
        if old {
            self.listing = Some(comments.listing(listing));
        }
        self.stale = false;
        let Some(list) = &self.listing else {
            return;
        };
        self.rows = rows(list, self.filter, &self.search, self.others);
        self.missed = None;
        let threads = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| match r {
                Row::Thread { item, .. } => Some((i, &list.items[*item].thread().root)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let next = threads
            .iter()
            .find(|(_, r)| Some(r.as_str()) == root.as_deref())
            .or_else(|| threads.iter().find(|&&(i, _)| i >= self.selected))
            .or_else(|| threads.last());
        self.selected = next.map_or(0, |&(i, _)| i);
    }

    /// What `key` does; `listing` while the list of files is still
    /// growing.
    pub(super) fn key(
        &mut self,
        (comments, listing): (&Comments, bool),
        keys: &Keys,
        key: Key,
        mods: Mods,
    ) -> Outcome {
        self.sync(comments, listing);
        let before = self.selected().map(str::to_owned);
        let outcome = self.act(keys, key, mods);
        // A filter or search dropping the selected thread moves it too.
        self.sync(comments, listing);
        match (outcome, self.selected()) {
            (Outcome::Stay, Some(root)) if before.as_deref() != Some(root) => {
                Outcome::Moved(root.to_owned())
            }
            (outcome, _) => outcome,
        }
    }

    /// While the search is typed, keys with ctrl still move and quit.
    fn act(&mut self, keys: &Keys, key: Key, mods: Mods) -> Outcome {
        if self.editing && !mods.contains(Mods::CTRL) {
            match key {
                Key::Esc => {
                    self.search.clear();
                    self.editing = false;
                }
                Key::Enter => self.editing = false,
                Key::Backspace => {
                    self.search.pop();
                }
                Key::Char(c) => self.search.extend(c.to_lowercase()),
                _ => return Outcome::Stay,
            }
            self.stale = true;
            return Outcome::Stay;
        }
        let page = isize::try_from(self.height).unwrap_or(isize::MAX);
        let plain = !mods.contains(Mods::CTRL) && !mods.contains(Mods::ALT);
        match (key, keys.action(true, key, mods)) {
            (Key::Esc, _) if !self.search.is_empty() => {
                self.search.clear();
                self.stale = true;
            }
            (Key::Esc | Key::Enter, _) | (_, Some(Act::Open)) => return Outcome::Diff,
            (Key::Char('f'), _) if plain => {
                self.filter = (self.filter + 1) % FILTERS.len();
                self.stale = true;
            }
            (Key::Char('F'), _) if plain => {
                self.filter = (self.filter + FILTERS.len() - 1) % FILTERS.len();
                self.stale = true;
            }
            (Key::Char('b'), _) if plain => {
                self.others = !self.others;
                self.stale = true;
            }
            (_, Some(Act::FilterTree)) => self.editing = true,
            (_, Some(Act::ScrollDown)) => self.step(1),
            (_, Some(Act::ScrollUp)) => self.step(-1),
            (_, Some(Act::PageDown | Act::HalfPageDown)) => self.step(page),
            (_, Some(Act::PageUp | Act::HalfPageUp)) => self.step(-page),
            (_, Some(Act::Top)) => self.step(isize::MIN),
            (_, Some(Act::Bottom)) => self.step(isize::MAX),
            // The tree's, which isn't shown.
            (_, Some(Act::Collapse | Act::MarkViewed)) => {}
            _ => return Outcome::Pass,
        }
        Outcome::Stay
    }

    /// The title's parts: every thread and the open ones, or with a filter
    /// or a search, how many of them it keeps and what it is.
    fn title(&self, list: &Listing) -> Vec<(String, Role)> {
        let [all, open] = list.counts;
        let mut parts = vec![(" Comments".to_owned(), Role::Plain)];
        if self.filter == 0 && self.search.is_empty() {
            let s = if all == 1 { "" } else { "s" };
            parts.push((format!(" · {all} thread{s}"), Role::Dim));
            parts.push((format!(" · {open} open"), Role::Dim));
            return parts;
        }
        let kept = self
            .rows
            .iter()
            .filter(|r| {
                matches!(r, Row::Thread { item, .. }
                    if !list.files[list.items[*item].group].conversation
                        && !list.files[list.items[*item].group].branches)
            })
            .count();
        parts.push((format!(" · {kept}/{all}"), Role::Dim));
        if self.filter > 0 {
            parts.push((format!(" · {}", FILTERS[self.filter].0), Role::Hint));
        }
        if !self.search.is_empty() {
            parts.push((format!(" · /{}", self.search), Role::Hint));
        }
        parts
    }

    /// Draws the panel into columns `0..width` of rows `top..top + height`,
    /// scrolled to show the selected thread and, for a file's first, its
    /// header; see [`tree::frame`] for `joined`.
    pub(super) fn draw(
        &mut self,
        grid: &mut Grid,
        (top, height, width): (usize, usize, usize),
        (comments, listing): (&Comments, bool),
        (focused, joined): (bool, bool),
        theme: &Theme,
    ) {
        if height < 3 || width < 5 {
            return;
        }
        self.sync(comments, listing);
        self.height = height - 2;
        let selected = matches!(self.rows.get(self.selected), Some(Row::Thread { .. }));
        if selected {
            let first = match self.selected.checked_sub(1).map(|i| &self.rows[i]) {
                Some(Row::File { .. }) => self.selected - 1,
                _ => self.selected,
            };
            if first < self.scroll {
                self.scroll = first;
            } else if self.selected >= self.scroll + self.height {
                self.scroll = self.selected + 1 - self.height;
            }
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(self.height));
        let Some(list) = &self.listing else {
            return;
        };
        let typing = self
            .editing
            .then_some((self.search.as_str(), "type to search · esc clear "));
        tree::frame(
            grid,
            (top, height, width),
            &self.title(list),
            typing,
            (focused, joined),
            theme,
        );
        let (x, right) = (2, width - 2);
        let mut c = Canvas::default();
        for row in self.rows.iter().skip(self.scroll).take(self.height) {
            list_row(&mut c, list, row, right - x);
        }
        for (i, cells) in c.rows().enumerate() {
            let y = top + 1 + i;
            paint(grid, x, y, right, cells, theme, Attributes::NONE);
            if selected && self.scroll + i == self.selected {
                grid.map_styles(1, y, width - 1, |s| theme.selected(s, focused));
            }
        }
    }
}

/// The rows of `list` that filter `filter` and `search` keep: each file's
/// header, then its threads; the threads on other branches last, with
/// `others`, else a row saying how many there are.
fn rows(list: &Listing, filter: usize, search: &str, others: bool) -> Vec<Row> {
    let mut rows = Vec::new();
    if list.waiting > 0 {
        let s = if list.waiting == 1 { "" } else { "s" };
        rows.push(Row::Note(format!(
            "finding the lines of comments in {} file{s}…",
            list.waiting
        )));
    }
    if list.items.is_empty() {
        if list.waiting == 0 {
            rows.push(Row::Note(
                "no comments yet: c comments on the line at the top".to_owned(),
            ));
        }
        return rows;
    }
    let elsewhere = |i: usize| list.files[list.items[i].group].branches;
    let folded = if others {
        0
    } else {
        (0..list.items.len()).filter(|&i| elsewhere(i)).count()
    };
    let kept: Vec<usize> = (0..list.items.len())
        .filter(|&i| (others || !elsewhere(i)) && keeps(list, &list.items[i], filter, search))
        .collect();
    if folded > 0 {
        let s = if folded == 1 { "" } else { "s" };
        rows.push(Row::Note(format!(
            "{folded} thread{s} on other branches · b lists them"
        )));
    }
    if kept.is_empty() {
        rows.push(Row::Note(if search.is_empty() {
            FILTERS[filter].1.to_owned()
        } else {
            format!("no thread matches /{search}")
        }));
    }
    for threads in kept.chunk_by(|&a, &b| list.items[a].group == list.items[b].group) {
        rows.push(Row::File {
            group: list.items[threads[0]].group,
            threads: threads.len(),
        });
        let mut above = None;
        for &item in threads {
            let author = list.items[item].thread().notes.first().map(|n| &n.author);
            rows.push(Row::Thread {
                item,
                author: author != above,
            });
            above = author;
        }
    }
    rows
}

/// Whether filter `filter` and `search` keep `item`: the search finds its
/// file's path, fuzzily, or what a comment's author or text has.
fn keeps(list: &Listing, item: &Item, filter: usize, search: &str) -> bool {
    let thread = item.thread();
    let state = match filter {
        // The filters are about threads, which general comments aren't.
        _ if list.files[item.group].conversation => filter == 0,
        1 => !thread.resolved,
        2 => item.rank == Rank::Draft,
        3 => thread.resolved,
        _ => true,
    };
    state
        && (search.is_empty()
            || fuzzy::score_close(search, &list.files[item.group].path).is_some()
            || thread.notes.iter().any(|n| {
                who(&n.author).to_lowercase().contains(search)
                    || n.text.to_lowercase().contains(search)
            }))
}

/// Writes `row` of the panel's list, `width` columns wide. A file header's
/// folders are cut from the left to leave its name whole. A thread's
/// replies are marked (`↩2`) so they don't read as the header's count of
/// threads.
fn list_row(c: &mut Canvas, list: &Listing, row: &Row, width: usize) {
    match row {
        Row::Note(text) => {
            cut(c, &[(Role::Dim, text.as_str())], width);
        }
        Row::File { group, threads }
            if list.files[*group].conversation || list.files[*group].branches =>
        {
            let title: &[(Ink, &str)] = if list.files[*group].conversation {
                &[(AUTHOR, "Conversation")]
            } else {
                &[(AUTHOR, "Other branches"), (DIM, " · B moves one here")]
            };
            spread(c, title, &[(Role::Dim, &threads.to_string())], width);
        }
        Row::File { group, threads } => {
            let group = &list.files[*group];
            let path = escape(group.path.as_bytes());
            let (dir, name) = path.split_at(path.rfind('/').map_or(0, |i| i + 1));
            let absent = if group.absent {
                " · not in this diff"
            } else {
                ""
            };
            let count = threads.to_string();
            let room = width.saturating_sub(count.width() + 2);
            let dir = tail(dir, room.saturating_sub(name.width() + absent.width()));
            spread(
                c,
                &[
                    (Role::Dim.into(), &dir),
                    (AUTHOR, name),
                    (Role::Dim.into(), absent),
                ],
                &[(Role::Dim, &count)],
                width,
            );
        }
        Row::Thread { item, author } => {
            let item = &list.items[*item];
            let thread = item.thread();
            let Some(first) = thread.notes.first() else {
                c.end_line();
                return;
            };
            let ink = |role: Role| -> Ink {
                if thread.resolved {
                    Role::Dim.into()
                } else {
                    role.into()
                }
            };
            let line = item
                .line
                .filter(|_| width >= LINE_COLUMNS)
                .map(|n| format!(":{n} "));
            let moved = Ink {
                attributes: Attributes::DIM,
                ..Role::Warn.into()
            };
            let (kind, label) = match &thread.at {
                At::Conversation { kind, label } => (shown_kind(kind.as_deref()), label.as_deref()),
                _ => (None, None),
            };
            let words = [
                (thread.at == At::Lost && !list.files[item.group].absent)
                    .then_some((moved, "moved")),
                thread.branch.as_deref().map(|b| (ink(Role::Hint), b)),
                thread.local.then_some((ink(Role::Hint), "local")),
                (first.standing == Standing::Unposted).then_some((ink(Role::Dim), "not posted")),
                kind.map(|k| (ink(Role::Dim), k)),
                label.map(|l| (ink(Role::Hint), l)),
            ];
            let name = author.then(|| who(&first.author));
            let mut spans = vec![
                (ink(Role::Plain), "  "),
                (ink(thread.look.role()), thread.look.glyph()),
                (ink(Role::Plain), " "),
            ];
            spans.extend(line.as_deref().map(|l| (Role::LineNumber.into(), l)));
            for (word_ink, word) in words.into_iter().flatten() {
                spans.extend([(word_ink, word), (ink(Role::Plain), " ")]);
            }
            if let Some(name) = &name {
                let bold = Ink {
                    attributes: Attributes::BOLD,
                    ..ink(Role::Plain)
                };
                spans.extend([(bold, name.as_str()), (ink(Role::Plain), " ")]);
            }
            spans.push((ink(Role::Plain), &first.summary));
            let replies = match thread.notes.len() - 1 {
                0 => String::new(),
                _ if width < REPLIES_COLUMNS => String::new(),
                n => format!("↩{n}"),
            };
            spread(c, &spans, &[(Role::Dim, &replies)], width);
        }
    }
    c.end_line();
}

/// The end of `text` that fits `width` columns, after `…` when that's not
/// all of it.
fn tail(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = 1;
    let mut start = text.len();
    for (i, ch) in text.char_indices().rev() {
        used += ch.width().unwrap_or(0);
        if used > width {
            break;
        }
        start = i;
    }
    format!("…{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use dffy_core::diff::{Options, hunks};

    use super::*;

    fn text(s: &str) -> Text {
        Text::new(s.as_bytes().to_vec())
    }

    /// A changed line has a number only on its own side; below a hunk, the
    /// other side's is shifted by what the hunk added.
    #[test]
    fn line_numbers_on_both_sides_for_unchanged_lines() {
        let old = text("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n");
        let new = text("a\nB\nB2\nc\nd\ne\nf\ng\nh\ni\nj\n");
        let hunks = hunks(
            &old,
            &new,
            &Options {
                context: 1,
                ..Options::default()
            },
        );
        let n = |side, line| {
            let n = numbers(&hunks, side, line);
            (n.old, n.new)
        };
        assert_eq!(n(Side::New, 0), (Some(0), Some(0)), "context in the hunk");
        assert_eq!(n(Side::New, 1), (None, Some(1)), "added");
        assert_eq!(n(Side::Old, 1), (Some(1), None), "removed");
        assert_eq!(n(Side::New, 9), (Some(8), Some(9)), "below the hunk");
        assert_eq!(n(Side::Old, 8), (Some(8), Some(9)));
    }

    /// Offsets and fractions count; past a month an age is the date, and
    /// what isn't a time shows as given.
    #[test]
    fn ages_read_rfc_3339_times() {
        let now = epoch("2026-11-17T12:00:00Z").expect("a time");
        assert_eq!(epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch("2026-11-17T13:00:00+01:00"), Some(now));
        assert_eq!(age("2026-11-17T11:59:30.123Z", now), "just now");
        assert_eq!(age("2026-11-17T11:55:00Z", now), "5m ago");
        assert_eq!(age("2026-11-17T10:00:00Z", now), "2h ago");
        assert_eq!(age("2026-11-14T12:00:00Z", now), "3d ago");
        assert_eq!(age("2026-09-01T12:00:00Z", now), "2026-09-01");
        assert_eq!(age("yesterday", now), "yesterday");
    }

    /// Stat decides, so the watcher's event for a write dffy already read
    /// back costs no read.
    #[test]
    fn a_refresh_reads_only_files_changed_since_the_last_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("comments.json");
        std::fs::write(&path, "{\"version\":1,\"notice\":\"a\",\"comments\":[]}").expect("write");
        let mut comments = Comments::new(path.clone(), None, false, None);
        assert_eq!(comments.refresh(), Ok(Some(Vec::new())), "the first read");
        let before = std::fs::metadata(&path)
            .expect("stat")
            .modified()
            .expect("mtime");
        // Unreadable, but with the same size, inode and time: never read.
        std::fs::write(&path, "{\"version\":1,\"notice\":\"a\",\"comments\":[x}").expect("write");
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open");
        file.set_modified(before).expect("set the time back");
        assert_eq!(
            comments.refresh(),
            Ok(None),
            "unchanged as far as stat tells"
        );
        file.set_modified(before + std::time::Duration::from_secs(1))
            .expect("touch");
        assert!(comments.refresh().is_err(), "read once it changed");
        assert_eq!(comments.refresh(), Ok(None), "a broken file is said once");
    }

    /// Two spellings of one file are one source, so posting never moves
    /// drafts from the file into itself, whose second lock never comes.
    #[test]
    fn the_local_review_is_dropped_when_it_is_the_comments_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        std::fs::create_dir(&real).expect("mkdir");
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("symlink");
        let path = real.join("comments.json");
        let sources =
            |local: PathBuf| Comments::new(path.clone(), Some(local), true, None).sources();
        let alone = std::slice::from_ref(&path);
        assert_eq!(
            sources(alias.join("comments.json")),
            alone,
            "not written yet"
        );
        std::fs::write(&path, "").expect("write");
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&path, &link).expect("symlink");
        assert_eq!(sources(link), alone);
        let other = real.join("other.json");
        assert_eq!(sources(other.clone()), [path.clone(), other]);
    }
}

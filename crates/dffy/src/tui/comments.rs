//! Review comments in the viewer: the comments file as last read, indexed by
//! path, the threads each file shows, the rows a thread or the text box
//! writing one draws inside the diff, and the panel listing every thread.
//!
//! Threads are placed when a file's diff loads or its comments change,
//! never while drawing. Files with comments are diffed in the background
//! (see [`Comments::pending`]) so every thread finds its row, or is known
//! not to. The diff draws a placed thread under its last line, a comment
//! on the whole file under the file's first row, and one whose lines are
//! gone under its last row; [`thread_rows`] writes what goes there.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;

use dffy_core::bstr::BString;
use dffy_core::diff::{Hunk, LineKind, Text};
use dffy_core::document::{File, Side};
use dffy_core::git::{FileChange, FileDiff, Status};
use dffy_core::highlight;
use dffy_core::view::{Attributes, Run, Spans};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::help;
use super::textbox::TextBox;
use super::view::Entry;
use crate::keys::{Action as Act, Keys};
use crate::render::{Canvas, escape};
use crate::review::{self, Anchor, Comment, LineNumbers, Op, Review};
use crate::term::{Grid, Key, Mods};
use crate::theme::{Ink, Role, Style, Theme};

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
}

/// A comment as its thread shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Note {
    pub id: String,
    /// Empty means you.
    pub author: String,
    pub created: String,
    pub text: String,
    pub suggestion: Option<String>,
    pub standing: Standing,
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
}

/// The code a thread was written on, drawn at the top of its box when its
/// lines aren't on screen: the commented lines between their context, with
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
    /// Line comments whose anchor matched nowhere.
    unplaced: Vec<String>,
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
    /// is deleted, as `d d` would.
    pub(super) fn into_op(self, text: &str) -> Option<(Op, &'static str)> {
        let text = text.trim();
        if text.is_empty() {
            return match self {
                Self::Edit { id, .. } => Some((Op::Rm(review::Target::Id(id)), "draft deleted")),
                Self::New(_) | Self::Reply(_) => None,
            };
        }
        Some(match self {
            Self::New(mut comment) => {
                text.clone_into(&mut comment.text);
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
            Self::Edit { before, .. } if before.trim() == text => return None,
            Self::Edit { id, .. } => (
                Op::Set {
                    target: review::Target::Id(id),
                    change: review::Change {
                        text: Some(text.to_owned()),
                        ..review::Change::default()
                    },
                },
                "comment saved",
            ),
        })
    }
}

/// The comments file shown and, on a review session's, the drafts of the
/// local review's file (your comments without a ref, and their threads).
/// A local draft shows only where it places on this diff, marked `local`;
/// edits, replies and deletes in its thread go to the local file, and
/// posting moves it to the session's first (see [`Comments::move_local`]).
pub(super) struct Comments {
    path: PathBuf,
    /// The local review's file, when it isn't `path`.
    local: Option<PathBuf>,
    /// `path` is a review session's: your comments without a ref in it
    /// are drafts.
    session: bool,
    /// `path`'s comments, then `local`'s drafts.
    review: Review,
    /// The ids of the comments in `review` from `local`.
    from_local: HashSet<String>,
    /// Each path's comments, replies included, as indices into `review`.
    by_path: HashMap<BString, Vec<usize>>,
    placed: HashMap<BString, Placed>,
    /// Where each commented path is in the list, when it is.
    located: HashMap<BString, usize>,
    /// What stat said of `path` and `local` just before the last read.
    stamps: Vec<Option<Stamp>>,
}

/// Modification time (seconds, nanoseconds), size and inode.
type Stamp = (i64, i64, u64, u64);

impl Comments {
    /// No comments until [`Comments::reload`] reads `path`, and `local`'s
    /// drafts when it's another file.
    pub(super) fn new(path: PathBuf, local: Option<PathBuf>, session: bool) -> Self {
        Self {
            local: local.filter(|l| *l != path),
            path,
            session,
            review: Review::default(),
            from_local: HashSet::new(),
            by_path: HashMap::new(),
            placed: HashMap::new(),
            located: HashMap::new(),
            stamps: Vec::new(),
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
        self.placed.clear();
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
        if let Some(local) = &self.local {
            let drafts = review::read(local)?.comments;
            // Yours only: posting moves these threads, and someone else's
            // would go out under your name.
            let roots: HashSet<String> = drafts
                .iter()
                .filter(|c| c.in_reply_to.is_none() && c.reference.is_none() && c.author.is_empty())
                .map(|c| c.id.clone())
                .collect();
            for c in drafts {
                if c.reference.is_none() && roots.contains(c.in_reply_to.as_ref().unwrap_or(&c.id))
                {
                    from_local.insert(c.id.clone());
                    review.comments.push(c);
                }
            }
        }
        self.from_local = from_local;
        if review == self.review {
            return Ok(None);
        }
        let by_path = index(&review);
        let of = |review: &Review, by_path: &HashMap<BString, Vec<usize>>, path: &BString| {
            by_path.get(path).map_or_else(Vec::new, |at| {
                at.iter().map(|&i| review.comments[i].clone()).collect()
            })
        };
        let mut changed: Vec<BString> = by_path
            .keys()
            .chain(self.by_path.keys().filter(|p| !by_path.contains_key(*p)))
            .filter(|p| of(&review, &by_path, p) != of(&self.review, &self.by_path, p))
            .cloned()
            .collect();
        changed.sort_unstable();
        for path in &changed {
            self.placed.remove(path);
        }
        self.review = review;
        self.by_path = by_path;
        Ok(Some(changed))
    }

    /// Applies `ops` to a file under its lock, then reads it back: the
    /// local review's when the first names a local draft by id (a reply
    /// in its thread, a change or a removal), else the comments file.
    /// Returns the paths whose comments changed and the id each operation
    /// touched.
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
        let file = match &self.local {
            Some(local) if named.is_some_and(|id| self.from_local.contains(id)) => local,
            _ => &self.path,
        };
        let ids = review::apply(file, ops)?;
        Ok((self.reload()?.unwrap_or_default(), ids))
    }

    /// The local drafts in view, replies included: what posting moves.
    fn local_shown(&self) -> impl Iterator<Item = &Comment> {
        (self.review.comments.iter()).filter(|c| self.from_local.contains(&c.id) && !self.hides(c))
    }

    /// How many drafts [`Comments::move_local`] would move.
    pub(super) fn local_drafts(&self) -> usize {
        if self.from_local.is_empty() {
            return 0;
        }
        (self.local_shown())
            .filter(|c| self.standing(c) == Standing::Draft)
            .count()
    }

    /// Moves the local drafts in view into the comments file, with new ids,
    /// and out of the local review's once that's written (see
    /// [`review::move_threads`]), then reads both again. Returns the paths
    /// whose comments changed.
    pub(super) fn move_local(&mut self) -> Result<Vec<BString>, String> {
        let roots: Vec<String> = (self.local_shown())
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
        self.located.clear();
        if self.by_path.is_empty() {
            return;
        }
        for (i, entry) in files.iter().enumerate() {
            let path = &entry.change().path;
            if !entry.is_commit() && self.by_path.contains_key(path) {
                self.located.insert(path.clone(), i);
            }
        }
    }

    /// Notes `entry`, just listed at `file`, if it has comments.
    pub(super) fn locate_one(&mut self, file: usize, entry: &Entry) {
        let path = &entry.change().path;
        if !entry.is_commit() && self.by_path.contains_key(path) {
            self.located.insert(path.clone(), file);
        }
    }

    /// Where `path` is in the list.
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
            .filter(|&path| self.placed.get(path).is_none_or(|p| !p.threads.is_empty()))
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
        let mut unplaced = Vec::new();
        let mut hidden = Vec::new();
        let roots = review::threads(indices.iter().map(|&i| &self.review.comments[i]));
        for comment in roots.into_iter().filter(|c| c.in_reply_to.is_none()) {
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
                        unplaced.push(comment.id.clone());
                        At::Lost
                    }
                }
            };
            threads.push(Thread {
                root: comment.id.clone(),
                look: self.look(comment),
                resolved: comment.resolved,
                outdated: comment.outdated,
                url: comment.url.clone(),
                notes: self
                    .thread(&comment.id)
                    .into_iter()
                    .map(|c| Note {
                        id: c.id.clone(),
                        author: c.author.clone(),
                        created: c.created.clone(),
                        text: c.text.clone(),
                        suggestion: c.suggestion.clone(),
                        standing: self.standing(c),
                    })
                    .collect(),
                lines: comment
                    .anchor
                    .as_ref()
                    .map(|a| a.lines.clone())
                    .unwrap_or_default(),
                snippet: (at == At::Lost)
                    .then(|| snippet(comment, highlight))
                    .flatten(),
                at,
                local,
            });
        }
        self.placed.insert(
            path.into(),
            Placed {
                version,
                threads: threads.into(),
                unplaced,
                hidden,
            },
        );
    }

    /// The threads of `path`, once placed.
    pub(super) fn threads(&self, path: &[u8]) -> Option<Arc<[Thread]>> {
        self.placed.get(path).map(|p| Arc::clone(&p.threads))
    }

    /// How many comments `path` has, and how the most pressing thread
    /// among them looks.
    pub(super) fn tally(&self, path: &[u8]) -> Option<(usize, Look)> {
        let shown: Vec<&Comment> = (self.by_path.get(path)?.iter())
            .map(|&i| &self.review.comments[i])
            .filter(|c| !self.hides(c))
            .collect();
        if shown.is_empty() {
            return None;
        }
        let look = (shown.iter())
            .filter(|c| c.in_reply_to.is_none())
            .map(|c| self.look(c))
            .max()
            .unwrap_or(Look::Open);
        Some((shown.len(), look))
    }

    /// Threads in the file shown and the local drafts in view, and the
    /// drafts among their comments.
    pub(super) fn counts(&self) -> (usize, usize) {
        let shown = || self.review.comments.iter().filter(|c| !self.hides(c));
        (
            shown().filter(|c| c.in_reply_to.is_none()).count(),
            shown()
                .filter(|c| self.standing(c) == Standing::Draft)
                .count(),
        )
    }

    /// Drafts to post, local ones in view included: new threads, and
    /// replies.
    pub(super) fn unsent(&self) -> (usize, usize) {
        let drafts = (self.review.comments.iter())
            .filter(|c| self.standing(c) == Standing::Draft && !self.hides(c));
        let replies = drafts.clone().filter(|c| c.in_reply_to.is_some()).count();
        (drafts.count() - replies, replies)
    }

    /// A thread's look, from its root.
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

    /// The root of comment `id`'s thread.
    pub(super) fn thread_root(&self, id: &str) -> Option<String> {
        let comment = self.find(id)?;
        Some(comment.in_reply_to.clone().unwrap_or_else(|| id.to_owned()))
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

    /// Every thread, by path and line, each with where it is when that's
    /// not on a line: on a whole file, on a file not in the diff, or whose
    /// lines weren't found. Also how many commented files are still to be
    /// placed; `listing` while the list of files is still growing.
    fn listed(&self, listing: bool) -> (Vec<(&Comment, Option<Why>)>, usize) {
        let mut out = Vec::new();
        let mut waiting: Vec<&str> = Vec::new();
        for root in review::threads(&self.review.comments)
            .into_iter()
            .filter(|c| c.in_reply_to.is_none())
        {
            let path = root.path.as_bytes();
            let local = self.from_local.contains(&root.id);
            if local && self.located.contains_key(path) && !self.placed.contains_key(path) {
                waiting.push(&root.path);
            } else if self.hides(root) {
                // A local draft that isn't on this diff is the local
                // review's, not this one's.
            } else if root.range().is_none() {
                out.push((root, Some(Why::File)));
            } else if !self.located.contains_key(path) {
                if listing {
                    waiting.push(&root.path);
                } else {
                    out.push((root, Some(Why::Absent)));
                }
            } else {
                match self.placed.get(path) {
                    Some(p) if p.unplaced.contains(&root.id) => out.push((root, Some(Why::Moved))),
                    Some(_) => out.push((root, None)),
                    None => waiting.push(&root.path),
                }
            }
        }
        waiting.sort_unstable();
        waiting.dedup();
        (out, waiting.len())
    }
}

/// Each path's comments, as indices into `review`.
fn index(review: &Review) -> HashMap<BString, Vec<usize>> {
    let mut by_path: HashMap<BString, Vec<usize>> = HashMap::new();
    for (i, comment) in review.comments.iter().enumerate() {
        by_path
            .entry(comment.path.as_bytes().into())
            .or_default()
            .push(i);
    }
    by_path
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
    let lines: Vec<&str> = (anchor.before.iter())
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
    static PAINTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Times [`painted`] ran on this thread, to tell that drawing never does.
#[cfg(test)]
pub(crate) fn painted_count() -> usize {
    PAINTED.with(std::cell::Cell::get)
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
fn plain(text: &str) -> String {
    escape(text.replace('\t', "    ").as_bytes())
}

/// `text` split at `runs`' edges, each piece in its syntax's ink; plain
/// without runs.
fn pieces(text: &str, runs: Option<&[Run]>) -> Vec<(Ink, String)> {
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

/// `you` for an empty author.
fn who(author: &str) -> String {
    if author.is_empty() {
        "you".to_owned()
    } else {
        escape(author.as_bytes())
    }
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

/// A comment's author in its heading.
const AUTHOR: Ink = Ink {
    role: Role::Plain,
    syntax: None,
    attributes: Attributes::BOLD,
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

/// The lines of a thread's box: light, heavy while it has the focus, and
/// dashed while it holds a draft. Shapes differ as well as colors: without
/// color the theme is attributes only.
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
        let (across, down) = match (draft, focused) {
            (false, false) => ("─", "│"),
            (true, false) => ("┄", "┆"),
            (false, true) => ("━", "┃"),
            (true, true) => ("┅", "┇"),
        };
        let corners = if focused {
            ["┏", "┓", "┗", "┛"]
        } else {
            ["┌", "┐", "└", "┘"]
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

impl Frame<'_> {
    /// Columns of text inside the padding.
    fn inner(&self) -> usize {
        self.width.saturating_sub(4).max(1)
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

    /// A folded thread's one row, between the box's sides: `left` cut to
    /// fit, and `right` at the right end when that leaves a dozen columns
    /// of `left`.
    fn card(&mut self, left: &[(Ink, &str)], right: &[(Role, &str)]) {
        let Border { role, down, .. } = self.border;
        let inner = self.inner();
        let right_width = match spans_width(right) {
            w if w > 0 && w + 2 + 12 <= inner => w,
            _ => 0,
        };
        let room = match right_width {
            0 => inner,
            w => inner - w - 2,
        };
        self.c.push(role, down);
        self.c.push(Role::Plain, " ");
        let used = cut(self.c, left, room);
        self.c.spaces(Role::Plain, inner - used - right_width);
        if right_width > 0 {
            cut(self.c, right, right_width);
        }
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

/// The words after a thread's first author: its state, then where it is
/// when that's not on a line, with the line it was on.
fn tags<'a>(thread: &'a Thread, first: &Note) -> Vec<(Role, &'a str)> {
    let mut out = Vec::new();
    out.extend(standing_tag(first.standing));
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
        (At::Lost, None) => out.push((Role::Warn, "lines not found")),
        (At::Line { .. }, _) => {}
    }
    out
}

/// Writes `thread`'s box, `width` columns wide: the first comment's
/// author, age and state in the top border, each reply's on a row of its
/// own after a blank one, every comment's text wrapped inside, and a
/// suggestion as the lines it replaces and its own. Authors are bold.
/// Collapsed, one row between the box's sides sums it up, and it returns
/// true: inks carry no background, so the caller tints that row.
/// `focused` draws the border heavy. `writing`, when the text box is in
/// this thread, takes the place of the comment it edits or follows the
/// last as a reply. `now` is seconds since 1970.
pub(crate) fn thread_rows(
    thread: &Thread,
    (collapsed, focused): (bool, bool),
    mut writing: Option<&mut Writing>,
    (width, now): (usize, u64),
    c: &mut Canvas,
) -> bool {
    let Some(first) = thread.notes.first() else {
        return false;
    };
    let border = Border::new(thread.look == Look::Draft, focused);
    let mut frame = Frame { c, border, width };
    if collapsed && writing.is_none() {
        let author = who(&first.author);
        let when = format!(" · {}", age(&first.created, now));
        let text = escape(first.text.lines().next().unwrap_or_default().as_bytes());
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
            (Role::Plain.into(), text.as_str()),
        ]);
        let count = match thread.notes.len() - 1 {
            0 => String::new(),
            1 => "1 reply".to_owned(),
            n => format!("{n} replies"),
        };
        frame.card(&spans, &[(Role::Dim, count.as_str())]);
        return true;
    }
    let inner = frame.inner();
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
            standing_tag(note.standing).into_iter().collect()
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
        if i == 0 {
            frame.top(&spans);
            if let Some(snippet) = &thread.snippet {
                frame.code(snippet);
            }
        } else {
            frame.heading(&spans);
        }
        if let Some(writing) = editing {
            frame.text_box(&mut writing.text);
            continue;
        }
        for text in wrap(&note.text, inner) {
            frame.body(&[(Role::Plain, &text)]);
        }
        if let Some(suggestion) = &note.suggestion {
            frame.body(&[(Role::Dim, "suggested change:")]);
            let old = thread
                .lines
                .iter()
                .map(|l| (Role::Removed, "- ", l.as_str()));
            let new = suggestion.lines().map(|l| (Role::Added, "+ ", l));
            for (role, sign, text) in old.chain(new) {
                let text = escape(text.replace('\t', "    ").as_bytes());
                frame.body(&[(role, sign), (role, &text)]);
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

/// Why a thread isn't on a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    /// A comment on the whole file.
    File,
    /// Its file isn't in this diff.
    Absent,
    /// Its lines weren't found where they were or near.
    Moved,
}

/// What a key in the panel asks of the viewer.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Stay,
    Close,
    /// Go to the thread with this root, and its file.
    Open(String),
}

/// The panel over the diff listing every thread; `enter` goes to the one
/// selected.
pub(super) struct Panel {
    selected: usize,
    scroll: usize,
    /// Rows the panel showed last, for paging.
    page: usize,
}

impl Panel {
    pub(super) fn new() -> Self {
        Self {
            selected: 0,
            scroll: 0,
            page: 1,
        }
    }

    pub(super) fn scroll_by(&mut self, rows: isize) {
        self.scroll = self.scroll.saturating_add_signed(rows);
    }

    /// What `key` does; keys the panel doesn't use close it, as in `?`.
    /// `listing` while the list of files is still growing.
    pub(super) fn key(
        &mut self,
        (comments, listing): (&Comments, bool),
        keys: &Keys,
        key: Key,
        mods: Mods,
    ) -> Outcome {
        let list = comments.listed(listing).0;
        let len = list.len();
        let page = self.page.max(1);
        match (key, keys.action(false, key, mods)) {
            (Key::Enter, _) => list
                .get(self.selected)
                .map_or(Outcome::Stay, |(c, _)| Outcome::Open(c.id.clone())),
            (Key::Down, _) | (_, Some(Act::ScrollDown)) => {
                self.selected = (self.selected + 1).min(len.saturating_sub(1));
                Outcome::Stay
            }
            (Key::Up, _) | (_, Some(Act::ScrollUp)) => {
                self.selected = self.selected.saturating_sub(1);
                Outcome::Stay
            }
            (Key::PageDown, _) | (_, Some(Act::PageDown | Act::HalfPageDown)) => {
                self.selected = (self.selected + page).min(len.saturating_sub(1));
                Outcome::Stay
            }
            (Key::PageUp, _) | (_, Some(Act::PageUp | Act::HalfPageUp)) => {
                self.selected = self.selected.saturating_sub(page);
                Outcome::Stay
            }
            _ => Outcome::Close,
        }
    }

    /// Draws the panel in the middle of the screen, the whole screen on a
    /// narrow one.
    pub(super) fn draw(
        &mut self,
        grid: &mut Grid,
        theme: &Theme,
        comments: &Comments,
        listing: bool,
    ) {
        let (gw, gh) = (grid.width(), grid.height());
        if gw < 12 || gh < 6 {
            return;
        }
        let full = gw < help::FULL_SCREEN_BELOW;
        let (width, height) = if full {
            (gw, gh)
        } else {
            ((gw - 4).min(100), gh - 2)
        };
        let (x0, y0) = ((gw - width) / 2, (gh - height) / 2);
        let end = x0 + width;
        let (x, right) = (x0 + 2, end - 3);
        let (list, waiting) = comments.listed(listing);
        self.selected = self.selected.min(list.len().saturating_sub(1));
        let lines = list_lines(comments, &list, waiting, right - x);
        help::frame(grid, (x0, y0, width, height), " comments ", theme);
        let top = y0 + 1;
        let rows = height.saturating_sub(3);
        self.page = rows;
        if let Some(at) = lines.iter().position(|l| l.item == Some(self.selected)) {
            if at < self.scroll {
                self.scroll = at;
            } else if at >= self.scroll + rows {
                self.scroll = at + 1 - rows;
            }
        }
        self.scroll = self.scroll.min(lines.len().saturating_sub(rows));
        for (i, line) in lines.iter().skip(self.scroll).take(rows).enumerate() {
            let mut style = theme.style(line.role);
            if line.item == Some(self.selected) {
                grid.fill(
                    x - 1,
                    top + i,
                    right + 1,
                    theme.selected(Style::PLAIN, true),
                );
                style = theme.selected(style, true);
                style.attributes |= Attributes::BOLD;
            }
            grid.put_str(x, top + i, right, &line.text, style);
        }
        help::scrollbar(
            grid,
            (end - 2, top, rows),
            (lines.len(), self.scroll),
            theme.style(Role::Dim),
        );
        grid.put_str(
            x,
            y0 + height - 2,
            right,
            "enter goes to it · esc close",
            theme.style(Role::Dim),
        );
    }
}

/// A row of the panel's text.
struct Line {
    text: String,
    role: Role,
    /// The thread it's of.
    item: Option<usize>,
}

/// Where a thread is: `path:12` on the new side, `path:12 (old)` on the
/// old one, the path alone for a file comment.
fn location(root: &Comment) -> String {
    let path = escape(root.path.as_bytes());
    match root.range() {
        Some((review::Side::New, lines)) => format!("{path}:{}", lines.start + 1),
        Some((review::Side::Old, lines)) => format!("{path}:{} (old)", lines.start + 1),
        None => path,
    }
}

fn list_lines(
    comments: &Comments,
    list: &[(&Comment, Option<Why>)],
    waiting: usize,
    width: usize,
) -> Vec<Line> {
    let mut lines = Vec::new();
    if waiting > 0 {
        let s = if waiting == 1 { "" } else { "s" };
        lines.push(Line {
            text: format!("finding the lines of comments in {waiting} file{s}…"),
            role: Role::Dim,
            item: None,
        });
    }
    if list.is_empty() && waiting == 0 {
        lines.push(Line {
            text: "no comments yet: c comments on the line at the top".to_owned(),
            role: Role::Dim,
            item: None,
        });
    }
    for (i, (root, why)) in list.iter().enumerate() {
        let state = match comments.standing(root) {
            _ if root.resolved => " · resolved",
            Standing::Draft => " · draft",
            Standing::Unposted => " · not posted",
            Standing::Posted => " · posted",
            Standing::Kept => "",
        };
        let note = match why {
            Some(Why::File) => " · on the file",
            Some(Why::Absent) => " · file not in this diff",
            Some(Why::Moved) => " · lines not found",
            None => "",
        };
        let first = root.text.lines().next().unwrap_or_default();
        let text = format!(
            "{}{state}{note} · {}",
            location(root),
            escape(first.as_bytes())
        );
        lines.push(Line {
            text: fit(&text, width),
            role: Role::Plain,
            item: Some(i),
        });
    }
    lines
}

/// `text` cut to `width` columns, with `…` when it's longer.
fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// `text`'s lines wrapped at spaces to `width` columns; a word longer than
/// that is broken where it reaches the edge.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    // `lines`, not a split on `\n`: hosts send CRLF.
    for line in text.lines() {
        let line = escape(line.replace('\t', "    ").as_bytes());
        let mut row = String::new();
        for word in line.split(' ') {
            let needed = word.width() + usize::from(!row.is_empty());
            if !row.is_empty() && row.width() + needed > width {
                out.push(std::mem::take(&mut row));
            }
            if !row.is_empty() {
                row.push(' ');
            }
            for c in word.chars() {
                if row.width() + c.width().unwrap_or(0) > width {
                    out.push(std::mem::take(&mut row));
                }
                row.push(c);
            }
        }
        out.push(row);
    }
    out
}

#[cfg(test)]
mod tests {
    use dffy_core::diff::{Options, hunks};

    use super::*;

    fn text(s: &str) -> Text {
        Text::new(s.as_bytes().to_vec())
    }

    /// A context line has a number on both sides; a changed one only on
    /// its own. Below a hunk, the other side is shifted by what it added.
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

    #[test]
    fn text_wraps_at_spaces_and_breaks_long_words() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\r\n\nb", 4), ["a", "", "b"]);
    }

    /// Ages count from RFC 3339 times, offsets and fractions included, and
    /// turn into the date after a month; what isn't a time shows as given.
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

    /// A refresh reads the files only once stat says one changed since the
    /// last read, so the watcher's event for a write dffy already read back
    /// costs no read.
    #[test]
    fn a_refresh_reads_only_files_changed_since_the_last_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("comments.json");
        std::fs::write(&path, "{\"version\":1,\"notice\":\"a\",\"comments\":[]}").expect("write");
        let mut comments = Comments::new(path.clone(), None, false);
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
}

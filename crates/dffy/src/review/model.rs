//! The comment threads a frontend shows: the comments file as last read,
//! merged with the local review's drafts on a review session, split by
//! branch, and each path's threads placed on a version of its diff.
//!
//! Comments on lines are the core's annotations: the core places them on
//! each file's diff off the main thread (see [`Comments::annotated`] and
//! [`Comments::pending`]), and the threads are made here from where they
//! landed, again at once when only the comments' text or standing changed.
//! Never while drawing. Files with comments are diffed in the background so
//! every thread finds its row, or is known not to. What a frontend draws of
//! them, and how, is its own, as are the keys its words name (see [`Hint`]).

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash as _, Hasher as _};
use std::ops::Range;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dffy_core::annotation::{Anchor, Annotated, Annotation, AnnotationSet, Placement};
use dffy_core::bstr::BString;
use dffy_core::diff::{Hunk, LineKind, Text};
use dffy_core::document::{self, File, Side};
use dffy_core::git::{FileChange, FileDiff, Status};
use dffy_core::highlight;
use dffy_core::view::{Run, Spans, Syntax};
use pulldown_cmark::{Event, Options, Parser, TagEnd};

use crate::render::plain;
use crate::review::{self, Comment, LineNumbers, Op, Review, who};

/// An action words point to, by what it does: a frontend says it with the
/// key that does it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hint {
    /// Fetch the review's new commits and threads.
    Update,
    /// Post the review, which submits what waits on the host.
    Submit,
    /// Open the review just posted on its host.
    OpenPosted,
    /// From the post dialog, go to the drafts that won't post as they are.
    GoToUnposted,
    /// Go to the next thread, or the one before.
    StepThreads,
    /// Quit again, stopping the post that runs.
    StopPosting,
    /// In the comments panel, list the threads on other branches.
    OtherBranches,
    /// Comment on the line at the top.
    Comment,
}

/// A piece of what the status bar, a note or a row says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Part {
    Text(String),
    Hint(Hint),
}

/// `words` as one line, each hint as `hint` reads it, with a frontend's
/// keys; a hint it has no words for is left out. Parts are joined with
/// ` · `, or a space after one ending in a colon.
pub(crate) fn say(words: &[Part], hint: impl Fn(Hint) -> Option<String>) -> String {
    let mut out = String::new();
    for part in words {
        let hinted;
        let text = match part {
            Part::Text(text) => text,
            Part::Hint(h) => match hint(*h) {
                Some(said) => {
                    hinted = said;
                    &hinted
                }
                None => continue,
            },
        };
        if !out.is_empty() {
            out.push_str(if out.ends_with(':') { " " } else { " · " });
        }
        out.push_str(text);
    }
    out
}

/// How a thread shows, least pressing first: a thread with a draft in it
/// is a draft, though its root is posted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Look {
    Resolved,
    Open,
    Draft,
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
        label: Option<String>,
    },
}

/// A comment as its thread shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThreadComment {
    pub id: String,
    /// Empty means you.
    pub author: String,
    pub created: String,
    /// Markdown.
    pub text: String,
    /// [`summary`] of the text.
    pub summary: String,
    pub suggestion: Option<String>,
    pub standing: Standing,
    /// On the host, but only for you: in a review not submitted yet.
    pub pending: bool,
    /// For a draft, what posting it does, when that's news: refused
    /// before, waiting for its lines, or going on the whole file.
    pub posting: Option<Posting>,
}

/// What posting a draft does, in the words its heading says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Posting {
    /// It won't go as it is: refused before, or left out by the plan.
    pub warn: bool,
    /// Which may hold the host's: escaped where they're drawn.
    pub words: String,
}

impl ThreadComment {
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
    pub comments: Vec<ThreadComment>,
    /// The commented lines' text, which a suggestion replaces.
    pub lines: Vec<String>,
    /// For a thread whose lines weren't found, the code it was written on.
    pub snippet: Option<Snippet>,
    /// A draft of the local review's file, shown on a review session's
    /// diff.
    pub local: bool,
    /// The branch of a thread written on a branch other than the current
    /// one, which only the comments panel lists.
    pub branch: Option<String>,
}

impl Thread {
    /// Whether it holds someone else's comment not posted, as an agent's,
    /// that `a` makes yours.
    pub(crate) fn unposted(&self) -> bool {
        self.comments
            .iter()
            .any(|n| n.standing == Standing::Unposted)
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
    /// Context around the commented lines, drawn dimmed.
    pub context: bool,
    /// The text as written, each piece with its syntax.
    pub pieces: Vec<(Option<Syntax>, String)>,
}

/// Where a path's annotations landed on its diff, as [`land`] places them.
#[derive(Debug, Clone)]
pub(crate) struct Landed {
    /// The annotation set's generation they were placed at.
    pub generation: u64,
    pub placements: Vec<Placement>,
    /// The syntax runs of each lost annotation's code, by its id, for its
    /// thread's snippet; `None` while some are lost and their code isn't
    /// parsed.
    pub painted: Option<HashMap<String, Spans>>,
}

/// Places `annotated` on `diff`, a line diff, and with `paint` parses the
/// code of those lost in their file's language. That takes milliseconds
/// per lost annotation, so the diff of a file on screen is placed without
/// it and a placement of its own paints after (see [`Comments::pending`]).
pub(crate) fn land(annotated: &Annotated, diff: &FileDiff, paint: bool) -> Landed {
    let placements = dffy_core::annotation::place(&annotated.annotations, diff);
    let lost = (placements.iter()).any(|p| matches!(p, Placement::Lost { .. }));
    let painted = match (lost, paint) {
        (false, _) => Some(HashMap::new()),
        (true, true) => Some(paint_lost(&annotated.annotations, &placements)),
        (true, false) => None,
    };
    Landed {
        generation: annotated.generation,
        placements,
        painted,
    }
}

/// The syntax runs of each lost annotation's anchored code (see [`land`]).
fn paint_lost(annotations: &[Annotation], placements: &[Placement]) -> HashMap<String, Spans> {
    let lost: HashSet<&str> = (placements.iter())
        .filter_map(|p| match p {
            Placement::Lost { id } => Some(id.as_str()),
            Placement::Placed { .. } => None,
        })
        .collect();
    (annotations.iter())
        .filter(|a| lost.contains(a.id.as_str()))
        .filter_map(|a| {
            let anchor = a.anchor.as_ref()?;
            let path = match (a.side, &a.old_path) {
                (Side::Old, Some(old)) => old,
                _ => &a.path,
            };
            Some((a.id.clone(), painted(path, &anchor_lines(anchor))?))
        })
        .collect()
}

/// An anchor's lines with their context, in order.
fn anchor_lines(anchor: &Anchor) -> Vec<&str> {
    (anchor.before.iter())
        .chain(&anchor.lines)
        .chain(&anchor.after)
        .map(String::as_str)
        .collect()
}

/// Where a path's comments were placed on one version of its entry, and
/// the threads made from them.
pub(super) struct Placed {
    version: u64,
    /// The annotation set's generation the placements were made at.
    generation: u64,
    placements: Vec<Placement>,
    painted: Option<HashMap<String, Spans>>,
    pub(super) threads: Arc<[Thread]>,
    /// Local drafts whose anchor matched nowhere, or not placed yet: out of
    /// view.
    hidden: Vec<String>,
}

/// What the text box's or the editor's text becomes, kept while either is
/// open.
#[derive(Clone)]
pub(crate) enum Compose {
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
    pub(crate) fn into_op(self, text: &str) -> Option<(Op, &'static str)> {
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
pub(crate) fn with_suggestion(text: &str, suggestion: Option<&str>) -> String {
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
pub(crate) struct Comments {
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
    pub(super) review: Review,
    /// The ids of the comments in `review` from `local`.
    pub(super) from_local: HashSet<String>,
    /// The threads on other branches, roots first, as the files have them;
    /// not in `review`.
    others: Vec<Comment>,
    /// The ids in `others` from `local`, whose writes go there.
    others_local: HashSet<String>,
    /// What posting each draft does, by id, when that's news: the words
    /// its heading says.
    plans: HashMap<String, Posting>,
    /// Renamed files in the list, by new path: their old path.
    renamed: HashMap<BString, BString>,
    /// Each path's comments, replies included, as indices into `review`.
    by_path: HashMap<BString, Vec<usize>>,
    /// Each comment's index in `review`, by id.
    index: HashMap<String, usize>,
    /// Each thread's replies, in file order, as indices into `review`, by
    /// its first comment's id.
    replies: HashMap<String, Vec<usize>>,
    /// The line comments of `review`, which core places on each file's
    /// diff.
    annotations: AnnotationSet,
    pub(super) placed: HashMap<BString, Placed>,
    /// Lost threads' code is highlighted.
    highlight: bool,
    /// Where each commented path is in the list, when it is.
    pub(super) located: HashMap<BString, usize>,
    /// What stat said of `path` and `local` just before the last read.
    stats: Vec<Option<Stat>>,
    /// The stamps of the bytes of `path` and `local` the last read parsed.
    stamps: Vec<Option<Stamp>>,
    /// Changes whenever the threads listed or placed may have: what the
    /// panel's list is kept against.
    pub(super) generation: u64,
    /// The general comments' threads, made when the conversation page or
    /// the panel first shows them after a read.
    conversation: OnceCell<Arc<[Thread]>>,
    /// See [`Comments::drafts_key`].
    drafts_key: u64,
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

/// A file's length and a hash of its bytes: unlike its stat, a file
/// replaced within one tick of the clock, its inode reused, still differs.
type Stamp = (usize, u64);

/// A file's modification time, size and inode.
type Stat = (i64, i64, u64, u64);

/// How [`Comments::refresh`] tells that a file changed since the last read.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Check {
    /// By its stat: cheap, and enough for dffy's own writes, which replace
    /// the file.
    Stat,
    /// By its bytes, which another program's write changes even when it
    /// keeps the size, the inode and the time, as one in the same instant
    /// may. The same bytes written again cost no parse.
    Bytes,
}

/// The stamp of a file holding `bytes`.
fn stamp(bytes: &[u8]) -> Stamp {
    // Keyed the same on every call, unlike `RandomState`.
    let mut hasher = std::hash::DefaultHasher::new();
    hasher.write(bytes);
    (bytes.len(), hasher.finish())
}

/// What a write left: the paths whose comments changed in the read back,
/// and why reading back failed, when it did. The write landed either way,
/// so writing it again would write it twice.
#[derive(Debug, Default)]
pub(crate) struct Written {
    pub changed: Vec<BString>,
    pub unread: Option<String>,
}

/// [`Comments::tally`] by path.
type Tallies = HashMap<BString, (usize, Look)>;

impl Comments {
    /// No comments until [`Comments::reload`] reads `path`, and `local`'s
    /// drafts when it's another file. `branch` is the current branch.
    pub(crate) fn new(
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
            others_local: HashSet::new(),
            plans: HashMap::new(),
            renamed: HashMap::new(),
            by_path: HashMap::new(),
            index: HashMap::new(),
            replies: HashMap::new(),
            annotations: AnnotationSet::default(),
            placed: HashMap::new(),
            highlight: false,
            located: HashMap::new(),
            stats: Vec::new(),
            stamps: Vec::new(),
            generation: 0,
            conversation: OnceCell::new(),
            drafts_key: 0,
            counted: Cell::new(None),
            pending_counted: Cell::new(None),
            tallied: RefCell::new(None),
            general_len: 0,
        }
    }

    /// The files read: the comments file, then the local review's.
    pub(crate) fn sources(&self) -> Vec<PathBuf> {
        std::iter::once(&self.path)
            .chain(&self.local)
            .cloned()
            .collect()
    }

    /// Whether `c` is a local draft that didn't place, or isn't placed
    /// yet: out of view.
    pub(super) fn hides(&self, c: &Comment) -> bool {
        // Checked per comment as the status bar draws.
        !self.from_local.is_empty()
            && self.from_local.contains(&c.id)
            && self
                .placed
                .get(c.path.as_bytes())
                .is_none_or(|p| p.hidden.contains(c.in_reply_to.as_ref().unwrap_or(&c.id)))
    }

    pub(crate) fn notice(&self) -> Option<&str> {
        self.review.notice.as_deref()
    }

    /// Takes the file as a review session's, or not. Returns the paths
    /// whose threads changed for it, none when it didn't change.
    pub(crate) fn set_session(&mut self, session: bool) -> Vec<BString> {
        if self.session == session {
            return Vec::new();
        }
        self.session = session;
        self.generation += 1;
        self.conversation = OnceCell::new();
        self.general_len = self.general().count();
        let paths: Vec<BString> = self.by_path.keys().cloned().collect();
        self.remake(&paths);
        paths
    }

    /// Lost threads' code is highlighted from the next threads made.
    pub(crate) fn set_highlight(&mut self, on: bool) {
        self.highlight = on;
    }

    pub(crate) fn highlight(&self) -> bool {
        self.highlight
    }

    fn standing(&self, c: &Comment) -> Standing {
        match (&c.reference, self.session, c.author.is_empty()) {
            (Some(_), ..) => Standing::Posted,
            (None, false, _) => Standing::Kept,
            (None, true, true) => Standing::Draft,
            (None, true, false) => Standing::Unposted,
        }
    }

    /// [`Comments::reload`], unless `check` finds neither file changed since
    /// the last read, as they don't after dffy's own writes, read back as
    /// they're made.
    pub(crate) fn refresh(&mut self, check: Check) -> Result<Option<Vec<BString>>, String> {
        let stats = self.stated();
        let unchanged = match check {
            Check::Stat => stats == self.stats,
            Check::Bytes => self.stamped() == self.stamps,
        };
        if !unchanged {
            return self.reload();
        }
        self.stats = stats;
        Ok(None)
    }

    fn stated(&self) -> Vec<Option<Stat>> {
        std::iter::once(&self.path)
            .chain(&self.local)
            .map(|path| {
                let m = std::fs::metadata(path).ok()?;
                Some((m.mtime(), m.mtime_nsec(), m.size(), m.ino()))
            })
            .collect()
    }

    fn stamped(&self) -> Vec<Option<Stamp>> {
        std::iter::once(&self.path)
            .chain(&self.local)
            .map(|path| review::read_bytes(path).ok().flatten().map(|b| stamp(&b)))
            .collect()
    }

    /// Reads the files back after a write of dffy's own.
    fn reread(&mut self) -> Written {
        match self.reload() {
            Ok(changed) => Written {
                changed: changed.unwrap_or_default(),
                unread: None,
            },
            Err(e) => Written {
                changed: Vec::new(),
                unread: Some(e),
            },
        }
    }

    /// Reads the files again. Returns the paths whose comments changed,
    /// `None` when nothing did; on an error in either the last read stays.
    pub(crate) fn reload(&mut self) -> Result<Option<Vec<BString>>, String> {
        // Before the read and of the bytes parsed, so a write landing after
        // either shows as a change next time. Kept on an error too, so a
        // broken file is reported once, not on every event.
        self.stats = self.stated();
        let read = |path: &Path| match review::read_bytes(path) {
            Ok(bytes) => (
                bytes.as_deref().map(stamp),
                review::parse_at(path, bytes.as_deref()),
            ),
            Err(e) => (None, Err(e)),
        };
        let (stamp, review) = read(&self.path);
        let local = self.local.as_deref().map(read);
        self.stamps = std::iter::once(stamp)
            .chain(local.as_ref().map(|(stamp, _)| *stamp))
            .collect();
        let mut review = review?;
        let mut from_local = HashSet::new();
        let mut others = Vec::new();
        let mut others_local = HashSet::new();
        if !self.session {
            let (here, elsewhere) =
                by_branch(std::mem::take(&mut review.comments), self.branch.as_deref());
            review.comments = here;
            others = elsewhere;
        }
        if let Some((_, drafts)) = local {
            let drafts = drafts?.comments;
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
            others_local = elsewhere.iter().map(|c| c.id.clone()).collect();
            others.extend(elsewhere);
        }
        self.from_local = from_local;
        self.others_local = others_local;
        // Before the check: the local drafts may change when the review
        // doesn't.
        self.generation += 1;
        let others_changed = others != self.others;
        self.others = others;
        if review == self.review {
            return Ok(others_changed.then(Vec::new));
        }
        let mut by_path: HashMap<BString, Vec<usize>> = HashMap::new();
        let mut index = HashMap::with_capacity(review.comments.len());
        let mut replies: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, comment) in review.comments.iter().enumerate() {
            // The first of an id the file repeats, as a search would find.
            index.entry(comment.id.clone()).or_insert(i);
            if let Some(root) = &comment.in_reply_to {
                replies.entry(root.clone()).or_default().push(i);
            }
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
        self.annotations
            .replace(review.comments.iter().filter_map(annotation));
        self.review = review;
        self.by_path = by_path;
        self.index = index;
        self.replies = replies;
        self.conversation = OnceCell::new();
        self.general_len = self.general().count();
        self.remake(&changed);
        // After the threads are made, which says which local drafts are
        // out of view.
        self.key_drafts();
        Ok(Some(changed))
    }

    /// Takes [`Comments::drafts_key`] as the drafts are now.
    fn key_drafts(&mut self) {
        let mut drafts = std::hash::DefaultHasher::new();
        for draft in self.drafts() {
            draft.hash(&mut drafts);
        }
        self.drafts_key = drafts.finish();
    }

    /// Applies `ops` to a file under its lock, then reads it back: the
    /// local review's when the first names a local draft by id (a reply
    /// in its thread, a change or a removal), or adds a thread of yours
    /// while new threads go there, else the comments file. Returns what
    /// the read back found and the id each operation touched; `Err` only
    /// when nothing was written.
    pub(crate) fn apply(&mut self, ops: Vec<Op>) -> Result<(Written, Vec<Option<String>>), String> {
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
        // Someone else's goes to the comments file even then, as the local
        // review's drafts shown are yours only.
        let new_thread = matches!(
            ops.first(),
            Some(Op::Add { reply_to: None, comment }) if comment.author.is_empty()
        );
        let local = named
            .is_some_and(|id| self.from_local.contains(id) || self.others_local.contains(id))
            || (new_thread && self.new_local);
        let file = match &self.local {
            Some(file) if local => file,
            _ => &self.path,
        };
        let ids = review::apply(file, ops)?;
        Ok((self.reread(), ids))
    }

    /// New threads go to the local review's file rather than the
    /// comments file, when there's one.
    pub(crate) fn set_new_local(&mut self, on: bool) {
        self.new_local = on && self.local.is_some();
    }

    /// Whether new threads written now go on the current branch: they go
    /// to the local review's file, which keeps branches apart.
    pub(crate) fn branched(&self) -> bool {
        !self.session || self.new_local
    }

    /// The summary being written for the review, kept in the comments file.
    pub(crate) fn summary(&self) -> Option<&str> {
        self.review.summary.as_deref()
    }

    /// Writes the review's summary to the comments file, unless it's what
    /// the file has already; empty removes it. Returns what the read back
    /// found, as another writer's may have changed threads.
    pub(crate) fn keep_summary(&mut self, text: &str) -> Result<Written, String> {
        let summary = Some(text.trim_end()).filter(|s| !s.is_empty());
        if summary == self.summary() {
            return Ok(Written::default());
        }
        review::apply(&self.path, vec![Op::Summary(summary.map(str::to_owned))])?;
        Ok(self.reread())
    }

    /// The path whose entry shows the thread with root `root` (see
    /// [`Comments::homes`]); empty for a general comment's.
    pub(crate) fn home_of(&self, root: &str) -> Option<&str> {
        self.find(root).map(|c| self.home(c))
    }

    /// The posted threads' roots, by `ref`: what a run importing from the
    /// host is compared with, before and after, to find the new ones.
    pub(crate) fn posted_roots(&self) -> HashMap<String, String> {
        self.review
            .comments
            .iter()
            .filter(|c| c.in_reply_to.is_none() && !c.path.is_empty())
            .filter_map(|c| Some((c.reference.clone()?, c.id.clone())))
            .collect()
    }

    /// How many of the comments shown are on the host for you only.
    pub(crate) fn host_pending(&self) -> usize {
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
    pub(crate) fn drafts(&self) -> impl Iterator<Item = (&str, bool, bool)> {
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
    }

    /// [`Comments::drafts`] as of the last read, hashed: when it changes
    /// over a read, their plan is asked for again.
    pub(crate) fn drafts_key(&self) -> u64 {
        self.drafts_key
    }

    /// How many local drafts in view were written before `epoch` (seconds
    /// since 1970), or at a time that doesn't read as one.
    pub(crate) fn local_drafts_before(&self, epoch: u64) -> usize {
        self.local_shown()
            .filter(|c| self.standing(c) == Standing::Draft)
            .filter(|c| self::epoch(&c.created).is_none_or(|t| t < epoch))
            .count()
    }

    /// Takes what posting each draft does, by id. Returns the paths whose
    /// threads are to be placed again for it.
    pub(crate) fn set_plans(&mut self, plans: HashMap<String, Posting>) -> Vec<BString> {
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
        self.plans = plans;
        self.generation += 1;
        self.conversation = OnceCell::new();
        self.remake(&paths);
        paths
    }

    /// Whether `root` starts a thread on another branch.
    pub(crate) fn elsewhere(&self, root: &str) -> bool {
        self.others.iter().any(|c| c.id == root)
    }

    /// Whether the host refused comment `id` when it was last posted.
    pub(crate) fn refused(&self, id: &str) -> bool {
        self.find(id).is_some_and(|c| c.post_error.is_some())
    }

    /// `roots` in the order the threads are listed: by file, then line.
    pub(crate) fn in_order(&self, roots: &HashSet<String>) -> Vec<String> {
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
    pub(crate) fn local_drafts(&self) -> usize {
        if self.from_local.is_empty() {
            return 0;
        }
        self.local_shown()
            .filter(|c| self.standing(c) == Standing::Draft)
            .count()
    }

    /// Moves the local drafts in view into the comments file, with new ids,
    /// and out of the local review's once that's written (see
    /// [`review::move_threads`]), then reads both again. Returns what the
    /// read back found.
    pub(crate) fn move_local(&mut self) -> Result<Written, String> {
        let roots = self.local_roots();
        let Some(local) = self.local.as_ref().filter(|_| !roots.is_empty()) else {
            return Ok(Written::default());
        };
        review::move_threads(local, &self.path, &roots)?;
        Ok(self.reread())
    }

    /// The first comments of the local drafts' threads in view.
    pub(crate) fn local_roots(&self) -> Vec<String> {
        self.local_shown()
            .filter(|c| c.in_reply_to.is_none())
            .map(|c| c.id.clone())
            .collect()
    }

    /// Finds the commented paths in the list `files`, after it changed
    /// order or content.
    pub(crate) fn locate<B>(&mut self, files: &[document::Entry<B>]) {
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
    pub(crate) fn locate_one<B>(&mut self, file: usize, entry: &document::Entry<B>) {
        if self.note_entry(file, entry) {
            self.generation += 1;
        }
    }

    /// Notes where `entry` is when it has comments, and a rename's old
    /// path; returns whether it has.
    fn note_entry<B>(&mut self, file: usize, entry: &document::Entry<B>) -> bool {
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
    pub(super) fn home<'a>(&self, root: &'a Comment) -> &'a str {
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

    pub(crate) fn file_of(&self, path: &[u8]) -> Option<usize> {
        self.located.get(path).copied()
    }

    /// The files in the list with comments, in list order; not those whose
    /// only comments are local drafts that didn't place.
    pub(crate) fn files(&self) -> Vec<usize> {
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
    pub(crate) fn paths(&self) -> HashSet<BString> {
        self.by_path
            .keys()
            .filter(|&path| {
                !path.is_empty() && self.placed.get(path).is_none_or(|p| !p.threads.is_empty())
            })
            .cloned()
            .collect()
    }

    /// The files whose comments aren't placed on their entry as it is now,
    /// were placed before they changed, or wait for their lost code to be
    /// painted, in list order: each is placed once its diff is in.
    pub(crate) fn pending<B>(&self, files: &[document::Entry<B>]) -> Vec<usize> {
        let mut out: Vec<usize> = self
            .located
            .iter()
            .filter(|&(path, &file)| {
                files.get(file).is_some_and(|e| {
                    self.placed.get(path).is_none_or(|p| {
                        p.version != e.version()
                            || !self.annotations.is_current(path, p.generation)
                            || self.highlight && p.painted.is_none()
                    })
                })
            })
            .map(|(_, &file)| file)
            .collect();
        out.sort_unstable();
        out
    }

    /// The line comments, as the core places them.
    pub(crate) fn annotations(&self) -> &AnnotationSet {
        &self.annotations
    }

    /// What to place on `path`'s diff, when it has comments: its line
    /// comments as annotations, none when it has only file comments.
    pub(crate) fn annotated(&self, path: &[u8]) -> Option<Annotated> {
        if !self.by_path.contains_key(path) {
            return None;
        }
        Some(
            self.annotations
                .on(path)
                .cloned()
                .unwrap_or_else(|| Annotated {
                    generation: self.annotations.generation(),
                    annotations: Arc::new([]),
                }),
        )
    }

    /// Takes where `path`'s comments landed on version `version` of its
    /// entry, and makes its threads. Returns the placements taken, and
    /// whether local drafts came into view or left it, which changes what
    /// posting sends; `None`, taking nothing, when the comments changed
    /// since they were placed.
    pub(crate) fn place(
        &mut self,
        path: &[u8],
        version: u64,
        landed: Landed,
    ) -> Option<(&[Placement], bool)> {
        let Landed {
            generation,
            placements,
            painted,
        } = landed;
        if !self.by_path.contains_key(path) || !self.annotations.is_current(path, generation) {
            return None;
        }
        let before = self.local_in_view(path);
        self.placed.insert(
            path.into(),
            Placed {
                version,
                generation,
                placements,
                painted,
                threads: Arc::new([]),
                hidden: Vec::new(),
            },
        );
        self.remake(&[path.into()]);
        let moved = before != self.local_in_view(path);
        if moved {
            self.key_drafts();
        }
        Some((&self.placed.get(path)?.placements, moved))
    }

    /// The roots of the local drafts on `path` in view.
    fn local_in_view(&self, path: &[u8]) -> Vec<String> {
        if self.from_local.is_empty() {
            return Vec::new();
        }
        (self.by_path.get(path).into_iter().flatten())
            .map(|&i| &self.review.comments[i])
            .filter(|c| c.in_reply_to.is_none() && self.from_local.contains(&c.id))
            .filter(|c| !self.hides(c))
            .map(|c| c.id.clone())
            .collect()
    }

    /// [`Comments::place`] without a diff to find lines in, on a version of
    /// `path`'s entry whose diff failed or that has no line comments: those
    /// it has are lost. Their code is parsed here, on a diff that failed
    /// only.
    pub(crate) fn place_without_diff(&mut self, path: &[u8], version: u64) {
        let Some(annotated) = self.annotated(path) else {
            return;
        };
        let placements: Vec<Placement> = (annotated.annotations.iter())
            .map(|a| Placement::Lost { id: a.id.clone() })
            .collect();
        // No placement job can paint a file without a diff later.
        let painted = Some(if self.highlight {
            paint_lost(&annotated.annotations, &placements)
        } else {
            HashMap::new()
        });
        let landed = Landed {
            generation: annotated.generation,
            placements,
            painted,
        };
        self.place(path, version, landed);
    }

    /// Makes the threads of each of `paths` placed, from its comments as
    /// they are now and its last placements. A line comment not placed yet
    /// is left out until it is.
    fn remake(&mut self, paths: &[BString]) {
        for path in paths {
            let Some(placed) = self.placed.get(path) else {
                continue;
            };
            let found: HashMap<&str, &Placement> =
                placed.placements.iter().map(|p| (p.id(), p)).collect();
            let mut threads = Vec::new();
            let mut hidden = Vec::new();
            let indices = self.by_path.get(path).map_or(&[][..], Vec::as_slice);
            let roots = review::threads(indices.iter().map(|&i| &self.review.comments[i]));
            for comment in roots
                .into_iter()
                .filter(|c| c.in_reply_to.is_none() && self.homes(c, path))
            {
                let local = self.from_local.contains(&comment.id);
                let at = match (comment.range(), found.get(comment.id.as_str())) {
                    (None, _) => At::File,
                    (Some(_), Some(Placement::Placed { side, lines, .. })) => At::Line {
                        side: *side,
                        lines: lines.clone(),
                    },
                    (Some(_), Some(Placement::Lost { .. })) if !local => At::Lost,
                    (Some(_), None) if !local => continue,
                    (Some(_), _) => {
                        hidden.push(comment.id.clone());
                        continue;
                    }
                };
                let painted = (placed.painted.as_ref())
                    .and_then(|p| p.get(&comment.id))
                    .filter(|_| self.highlight);
                threads.push(self.thread_of(comment, at, painted));
            }
            if let Some(placed) = self.placed.get_mut(path) {
                placed.threads = threads.into();
                placed.hidden = hidden;
            }
        }
        if !paths.is_empty() {
            self.generation += 1;
        }
    }

    /// The thread whose first comment is `root`, shown `at`. `painted`
    /// colors the code of one whose lines weren't found (see [`land`]).
    pub(super) fn thread_of(&self, root: &Comment, at: At, painted: Option<&Spans>) -> Thread {
        self.thread_from(root, self.thread(&root.id), at, painted)
    }

    /// The thread of `comments`, `root` first, shown `at`.
    fn thread_from(
        &self,
        root: &Comment,
        comments: Vec<&Comment>,
        at: At,
        painted: Option<&Spans>,
    ) -> Thread {
        Thread {
            root: root.id.clone(),
            look: self.look(root),
            resolved: root.resolved,
            outdated: root.outdated,
            url: root.url.clone(),
            comments: comments
                .into_iter()
                .map(|c| ThreadComment {
                    id: c.id.clone(),
                    author: c.author.clone(),
                    created: c.created.clone(),
                    text: c.text.clone(),
                    summary: summary(&c.text),
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
            snippet: (at == At::Lost).then(|| snippet(root, painted)).flatten(),
            local: self.from_local.contains(&root.id),
            branch: None,
            at,
        }
    }

    /// What posting draft `c` does, when that's news: the host refused it
    /// before, else what the last plan said.
    fn posting(&self, c: &Comment) -> Option<Posting> {
        if self.standing(c) != Standing::Draft {
            return None;
        }
        match &c.post_error {
            Some(why) => Some(Posting {
                warn: true,
                words: format!("refused: {why}"),
            }),
            None => self.plans.get(&c.id).cloned(),
        }
    }

    /// The threads on other branches, for the comments panel: each with
    /// its branch, on its file.
    pub(super) fn elsewhere_threads(&self) -> Vec<Thread> {
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
                    branch: root.branch.clone(),
                    ..self.thread_from(root, comments, at, None)
                }
            })
            .collect()
    }

    /// The threads of `path`, once placed.
    pub(crate) fn threads(&self, path: &[u8]) -> Option<Arc<[Thread]>> {
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
    pub(crate) fn conversation_len(&self) -> usize {
        self.general_len
    }

    /// The conversation page's threads, one per general comment: the
    /// review's description first, then the rest by when they were written.
    /// Made on the first call after a read.
    pub(crate) fn conversation(&self) -> Arc<[Thread]> {
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
                        label: c.label.clone(),
                    };
                    self.thread_of(c, at, None)
                })
                .collect()
        }))
    }

    /// How many comments `path` has, and how the most pressing thread
    /// among them looks.
    pub(crate) fn tally(&self, path: &[u8]) -> Option<(usize, Look)> {
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
    pub(crate) fn counts(&self) -> (usize, usize) {
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
        if std::iter::once(root)
            .chain(self.replies_of(root))
            .any(|c| self.standing(c) == Standing::Draft)
        {
            Look::Draft
        } else {
            Look::Open
        }
    }

    /// The replies in the thread whose first comment is `root`, in file
    /// order: on its path, as a file whose ids repeat may have another
    /// thread's under the same id.
    fn replies_of<'a>(&'a self, root: &'a Comment) -> impl Iterator<Item = &'a Comment> {
        (self.replies.get(&root.id).into_iter().flatten())
            .map(|&i| &self.review.comments[i])
            .filter(|c| c.path == root.path)
    }

    fn find(&self, id: &str) -> Option<&Comment> {
        self.index.get(id).map(|&i| &self.review.comments[i])
    }

    pub(crate) fn thread_root(&self, id: &str) -> Option<String> {
        self.find(id)
            .map(|c| c.in_reply_to.clone().unwrap_or_else(|| id.to_owned()))
    }

    /// The thread whose root is `root`, root first, replies in file order;
    /// empty when it's gone.
    pub(crate) fn thread(&self, root: &str) -> Vec<&Comment> {
        let Some(first) = self.find(root) else {
            return Vec::new();
        };
        std::iter::once(first)
            .chain(self.replies_of(first))
            .collect()
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

/// A comments file's threads as read once, for the frontends that place
/// them on each file's diff as they make it and never see the file change:
/// printed output, `--json` and MCP. It's immutable, so the threads diffing
/// files share it. General comments aren't on a file, so it has none.
pub(crate) struct Snapshot {
    comments: Vec<Comment>,
    /// Indices into `comments` of the threads' first comments, in thread
    /// order, by the path whose diff shows them. A line comment on a
    /// renamed file's old side is also under its `old_path`, for a diff
    /// that lists the rename as a deletion and an addition.
    roots: HashMap<BString, Vec<usize>>,
    /// Indices of each thread's replies, in the order they were added, by
    /// its first comment's id.
    replies: HashMap<String, Vec<usize>>,
}

/// A thread placed on a file's diff by [`Snapshot::on`].
#[derive(Debug)]
pub(crate) struct Hung<'a> {
    /// Its first comment, then its replies in the order they were added.
    pub thread: Vec<&'a Comment>,
    /// [`At::File`], [`At::Line`] on the side shown, or [`At::Lost`].
    pub at: At,
}

impl Snapshot {
    /// The threads of `review` that `branches` keeps.
    pub(crate) fn new(review: &Review, branches: review::Branches) -> Self {
        let select = review::Select {
            branches,
            ..review::Select::default()
        };
        let comments: Vec<Comment> = review::threads(review.selected(&select))
            .into_iter()
            .filter(|c| !c.path.is_empty())
            .cloned()
            .collect();
        let ids: HashSet<String> = comments.iter().map(|c| c.id.clone()).collect();
        let mut roots: HashMap<BString, Vec<usize>> = HashMap::new();
        let mut replies: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, c) in comments.iter().enumerate() {
            if let Some(root) = c.in_reply_to.as_ref().filter(|r| ids.contains(*r)) {
                replies.entry(root.clone()).or_default().push(i);
                continue;
            }
            roots.entry(c.path.as_bytes().into()).or_default().push(i);
            if let (Some(old), Some((review::Side::Old, _))) = (&c.old_path, c.range())
                && *old != c.path
            {
                roots.entry(old.as_bytes().into()).or_default().push(i);
            }
        }
        Self {
            comments,
            roots,
            replies,
        }
    }

    /// The path `change` (reversed with `-R` when `reverse`) has its threads
    /// under, and whether its diff has the side the threads were written on
    /// as old.
    fn key(change: &FileChange, reverse: bool) -> (&[u8], bool) {
        if reverse {
            (change.old_path(), change.new.is_some())
        } else {
            (&change.path, change.old.is_some())
        }
    }

    /// Whether `change`'s file has threads, so its diff is worth placing
    /// them on.
    pub(crate) fn has(&self, change: &FileChange, reverse: bool) -> bool {
        self.roots.contains_key(Self::key(change, reverse).0)
    }

    /// The threads on `change`'s file, in thread order, placed on `diff`,
    /// its line diff. With `reverse` (`-R`) the change and its diff are
    /// reversed, so a thread on the new side shows on the old one.
    ///
    /// A thread on a renamed file's old side shows on the rename, or, when
    /// the diff lists the rename as a deletion and an addition, on the
    /// deletion only: the addition has no old side.
    pub(crate) fn on(&self, change: &FileChange, diff: &FileDiff, reverse: bool) -> Vec<Hung<'_>> {
        let (key, has_old) = Self::key(change, reverse);
        let Some(roots) = self.roots.get(key) else {
            return Vec::new();
        };
        let roots: Vec<&Comment> = (roots.iter())
            .map(|&i| &self.comments[i])
            .filter(|c| {
                let renamed_old = matches!(c.range(), Some((review::Side::Old, _)))
                    && c.old_path.as_ref().is_some_and(|o| *o != c.path);
                has_old || !renamed_old || c.path.as_bytes() != key
            })
            .collect();
        let annotations: Vec<Annotation> = (roots.iter())
            .filter_map(|c| annotation(c))
            .map(|a| Annotation {
                side: match (a.side, reverse) {
                    (side, false) => side,
                    (Side::Old, true) => Side::New,
                    (Side::New, true) => Side::Old,
                },
                ..a
            })
            .collect();
        let placed: HashMap<String, Placement> = dffy_core::annotation::place(&annotations, diff)
            .into_iter()
            .map(|p| (p.id().to_owned(), p))
            .collect();
        roots
            .into_iter()
            .map(|root| {
                let at = match placed.get(&root.id) {
                    None => At::File,
                    Some(Placement::Placed { side, lines, .. }) => At::Line {
                        side: *side,
                        lines: lines.clone(),
                    },
                    Some(Placement::Lost { .. }) => At::Lost,
                };
                let replies = self.replies.get(&root.id).into_iter().flatten();
                Hung {
                    thread: std::iter::once(root)
                        .chain(replies.map(|&i| &self.comments[i]))
                        .collect(),
                    at,
                }
            })
            .collect()
    }
}

/// The source the comments' annotations name, in the core and in
/// `--json`'s and MCP's `annotations`.
pub(crate) const SOURCE: &str = "comments";

/// `c` as core places it, when it's on lines.
fn annotation(c: &Comment) -> Option<Annotation> {
    let (side, lines) = c.range()?;
    Some(Annotation {
        id: c.id.clone(),
        source: SOURCE,
        path: c.path.as_bytes().into(),
        old_path: c.old_path.as_deref().map(|p| p.as_bytes().into()),
        side: match side {
            review::Side::Old => Side::Old,
            review::Side::New => Side::New,
        },
        lines,
        anchor: c.anchor.clone(),
    })
}

/// The line diff of a loaded file, the source of a rendered view: what
/// comments are anchored in.
pub(crate) fn line_diff(file: &File) -> &Arc<FileDiff> {
    file.rendered().map_or_else(|| file.diff(), |r| r.source())
}

/// A new comment, without its text, on `lines` (0-based, non-empty) of
/// `side` of `diff`, the line diff of `change`.
pub(crate) fn draft(
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
pub(crate) fn file_draft(change: &FileChange) -> Result<Comment, &'static str> {
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
/// comment; `None` for a comment on the whole file. `painted`, the syntax
/// runs of its anchor's lines with their context (see [`land`]), colors
/// them.
///
/// A line's sign is its side's (`+` new, `-` old) unless the comment says
/// it was unchanged: dffy numbers such a line on both sides. Hosts number
/// one side only, so their threads always show the side's sign, and lines
/// between a range's first and last take it unless both ends were
/// unchanged.
pub(crate) fn snippet(comment: &Comment, painted: Option<&Spans>) -> Option<Snippet> {
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
    let lines = anchor_lines(anchor);
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
                    context: true,
                    pieces: vec![(None, (*text).to_owned())],
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
                context: false,
                pieces: pieces(text, painted.map(|s| s.line(i))),
            }
        })
        .collect();
    Some(Snippet { heading, rows })
}

/// [`snippet`] with its code parsed here, as [`land`] parses it.
#[cfg(test)]
pub(crate) fn painted_snippet(comment: &Comment) -> Option<Snippet> {
    let a = annotation(comment)?;
    let lost = [Placement::Lost { id: a.id.clone() }];
    let painted = paint_lost(std::slice::from_ref(&a), &lost);
    snippet(comment, painted.get(&a.id))
}

#[cfg(test)]
thread_local! {
    static PAINTED: Cell<usize> = const { Cell::new(0) };
}

/// Times [`painted`] ran on this thread, to tell that drawing and making
/// threads never do.
#[cfg(test)]
pub(crate) fn painted_count() -> usize {
    PAINTED.with(Cell::get)
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
fn painted(path: &[u8], lines: &[&str]) -> Option<Spans> {
    #[cfg(test)]
    PAINTED.with(|n| n.set(n.get() + 1));
    let mut bytes = Vec::new();
    for line in lines {
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
    }
    let lang = crate::grammars().detect(path, &bytes[..bytes.len().min(256)])?;
    let text = Text::new(bytes);
    Some(highlight::parse(lang, text.bytes())?.spans(&text, 0..text.len()))
}

/// `text` split at `runs`' edges, each piece with its syntax; one piece
/// without runs.
pub(crate) fn pieces(text: &str, runs: Option<&[Run]>) -> Vec<(Option<Syntax>, String)> {
    let Some(runs) = runs else {
        return vec![(None, text.to_owned())];
    };
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut push = |syntax: Option<Syntax>, range: Range<usize>| {
        if !range.is_empty() {
            out.push((syntax, String::from_utf8_lossy(&bytes[range]).into_owned()));
        }
    };
    let mut at = 0;
    for run in runs {
        let (start, end) = (run.start as usize, (run.end as usize).min(bytes.len()));
        if start < at || start >= end {
            continue;
        }
        push(None, at..start);
        push(Some(run.syntax), start..end);
        at = end;
    }
    push(None, at..bytes.len());
    out
}

/// The Markdown comments are written in: `CommonMark` with GitHub's
/// tables, strikethrough and task lists.
pub(crate) fn markdown_options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

/// What a folded thread and the comments panel show of a comment: the
/// first line of its text as rendered, without styles; escaped.
pub(crate) fn summary(text: &str) -> String {
    let mut out = String::new();
    // A block's comment may run over several of its lines.
    let mut html = String::new();
    for event in Parser::new_ext(text, markdown_options()) {
        match &event {
            Event::Text(t) | Event::Code(t) => out.push_str(t),
            Event::InlineHtml(h) => out.push_str(&uncomment(h)),
            Event::Html(h) => html.push_str(h),
            Event::End(TagEnd::HtmlBlock) => out.push_str(&uncomment(&std::mem::take(&mut html))),
            Event::TaskListMarker(done) => out.push_str(checkbox(*done)),
            Event::SoftBreak
            | Event::HardBreak
            | Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::Item
                | TagEnd::CodeBlock
                | TagEnd::TableCell,
            ) => out.push('\n'),
            _ => {}
        }
        if let Some((first, _)) = out.trim_start().split_once('\n') {
            return plain(first.trim());
        }
    }
    plain(out.trim())
}

/// A comment's text for a frontend that shows it without styles: its lines
/// as rendered, paragraphs and blocks a blank line apart, list items and
/// quotes marked, code indented as written, a link's address after its
/// text, HTML comments left out; each line [`plain`], unwrapped.
pub(crate) fn plain_text(text: &str) -> Vec<String> {
    use pulldown_cmark::Tag;
    let mut l = Lines::default();
    let (mut lists, mut links): (Vec<Option<u64>>, Vec<(String, usize)>) = (Vec::new(), Vec::new());
    let (mut code, mut html) = (false, String::new());
    for event in Parser::new_ext(text, markdown_options()) {
        match event {
            Event::Text(t) if code => {
                for line in t.split_inclusive('\n') {
                    l.push("    ");
                    l.push(line.strip_suffix('\n').unwrap_or(line));
                    if line.ends_with('\n') {
                        l.end();
                    }
                }
            }
            Event::Text(t) | Event::Code(t) => l.push(&t),
            Event::InlineHtml(h) => l.push(&uncomment(&h)),
            Event::Html(h) => html.push_str(&h),
            Event::End(TagEnd::HtmlBlock) => {
                for line in uncomment(&std::mem::take(&mut html)).lines() {
                    if !line.trim().is_empty() {
                        l.push(line);
                        l.end();
                    }
                }
            }
            Event::TaskListMarker(done) => l.push(checkbox(done)),
            Event::SoftBreak
            | Event::HardBreak
            | Event::End(TagEnd::TableRow | TagEnd::TableHead | TagEnd::Item) => l.end(),
            Event::End(TagEnd::TableCell) => l.push(" | "),
            Event::Rule => {
                l.blank();
                l.push("---");
                l.blank();
            }
            Event::Start(Tag::List(start)) => {
                l.end();
                lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
                if lists.is_empty() {
                    l.blank();
                }
            }
            Event::Start(Tag::Item) => {
                l.end();
                let marker = match lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    _ => "- ".to_owned(),
                };
                l.push(&"  ".repeat(lists.len().saturating_sub(1)));
                l.push(&marker);
            }
            Event::End(TagEnd::Paragraph) if !lists.is_empty() => l.end(),
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Table) => l.blank(),
            Event::Start(Tag::CodeBlock(_)) => {
                l.end();
                code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                code = false;
                l.blank();
            }
            Event::Start(Tag::BlockQuote(_)) => {
                l.end();
                l.quote += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                l.end();
                l.quote = l.quote.saturating_sub(1);
                l.blank();
            }
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                links.push((dest_url.into_string(), l.line.len()));
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                if let Some((dest, from)) = links.pop()
                    && !dest.is_empty()
                    && l.line.get(from..) != Some(dest.as_str())
                {
                    l.push(&format!(" <{dest}>"));
                }
            }
            _ => {}
        }
    }
    l.end();
    while l.out.last().is_some_and(String::is_empty) {
        l.out.pop();
    }
    l.out.iter().map(|line| plain(line)).collect()
}

/// The lines [`plain_text`] makes, the one being written last.
#[derive(Default)]
struct Lines {
    out: Vec<String>,
    line: String,
    /// `line` has its quote marks, so it's a line even while empty.
    started: bool,
    /// Block quotes the text is in.
    quote: usize,
}

impl Lines {
    fn push(&mut self, text: &str) {
        if !self.started {
            self.line.push_str(&"> ".repeat(self.quote));
            self.started = true;
        }
        self.line.push_str(text);
    }

    fn end(&mut self) {
        if self.started {
            self.out.push(std::mem::take(&mut self.line));
            self.started = false;
        }
    }

    /// Ends the line, then parts what comes next with a blank one.
    fn blank(&mut self) {
        self.end();
        if self.out.last().is_some_and(|l| !l.is_empty()) {
            self.out.push(String::new());
        }
    }
}

/// How long ago `created` was at `now` (seconds since 1970): `just now`,
/// `5m ago` up to `29d ago`, then its date.
pub(crate) fn age(created: &str, now: u64) -> String {
    let date = || created.get(..10).unwrap_or(created).to_owned();
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

pub(crate) fn checkbox(done: bool) -> &'static str {
    if done { "☑ " } else { "☐ " }
}

/// `html` less its comments, an unclosed one to the end.
pub(crate) fn uncomment(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 4..];
        rest = after.find("-->").map_or("", |end| &after[end + 3..]);
    }
    out.push_str(rest);
    out
}

/// `thread` as text to paste: each comment as written, under a line of its
/// author and the day it was written, a blank line apart.
pub(crate) fn thread_text(thread: &Thread) -> String {
    let comments: Vec<String> = thread
        .comments
        .iter()
        .map(|comment| {
            let day = comment.created.get(..10).unwrap_or(&comment.created);
            let text = comment.text.trim_end_matches(['\n', '\r']);
            format!("{} · {day}\n{text}", who(&comment.author))
        })
        .collect();
    comments.join("\n\n")
}

/// Seconds since 1970, for the threads' ages; fixed in tests, at noon on
/// 2026-11-17 UTC, so snapshots keep their ages.
#[cfg(not(test))]
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
pub(crate) fn now() -> u64 {
    1_794_916_800
}

/// Whether `url` is a web link, the only kind a thread's link opens as:
/// handed to the platform's opener, anything else could start a program.
pub(crate) fn is_web(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// Seconds since 1970 at `created`, an RFC 3339 time; `None` when it isn't
/// one.
pub(crate) fn epoch(created: &str) -> Option<u64> {
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

    /// A folded thread's line: the first line of text, past HTML comments and
    /// without its markup.
    #[test]
    fn summaries_are_the_first_line_of_text() {
        assert_eq!(
            summary(
                "<!-- cr-status -->\n<!-- cr-state\n{} -->\n\n@ana **Needs changes** at x.\nmore"
            ),
            "@ana Needs changes at x."
        );
        assert_eq!(summary("## Title\n\nbody"), "Title");
        assert_eq!(summary("- [x] Feature\n- [ ] Bug"), "☑ Feature");
        assert_eq!(summary("```rust\nfn main() {}\n```"), "fn main() {}");
        assert_eq!(summary("plain\r\nsecond"), "plain");
        assert_eq!(summary("a\u{1b}[31m"), "a^[[31m");
        assert_eq!(summary("<!-- nothing else -->"), "");
    }

    /// With the bytes deciding, the watcher's event for a write dffy already
    /// read back costs no parse, and one that keeps the size, the inode and
    /// the time, as a file replaced in the same instant may, still shows;
    /// the stat alone misses that one.
    #[test]
    fn a_refresh_reads_only_files_changed_since_the_last_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("comments.json");
        let unchanged = "{\"version\":1,\"notice\":\"a\",\"comments\":[]}";
        std::fs::write(&path, unchanged).expect("write");
        let mut comments = Comments::new(path.clone(), None, false, None);
        assert_eq!(
            comments.refresh(Check::Stat),
            Ok(Some(Vec::new())),
            "the first read"
        );
        std::fs::write(&path, unchanged).expect("write");
        let before = std::fs::metadata(&path)
            .expect("stat")
            .modified()
            .expect("mtime");
        assert_eq!(comments.refresh(Check::Bytes), Ok(None), "the same bytes");
        // Unreadable, with the same size, inode and time.
        std::fs::write(&path, "{\"version\":1,\"notice\":\"a\",\"comments\":[x}").expect("write");
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open");
        file.set_modified(before).expect("set the time back");
        assert_eq!(comments.refresh(Check::Stat), Ok(None), "the same stat");
        assert!(
            comments.refresh(Check::Bytes).is_err(),
            "read once it changed"
        );
        assert_eq!(
            comments.refresh(Check::Bytes),
            Ok(None),
            "a broken file is said once"
        );
    }

    /// A write that landed is reported as written even when reading the
    /// files back failed, which is said apart: writing it again would add
    /// it twice.
    #[test]
    fn a_write_that_landed_is_written_though_the_read_back_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, local) = (dir.path().join("7.json"), dir.path().join("local.json"));
        std::fs::write(&local, "{\"version\":1,\"comments\":[]}").expect("write");
        let mut comments = Comments::new(path.clone(), Some(local.clone()), true, None);
        comments.reload().expect("reads");
        std::fs::write(&local, "{broken").expect("write");
        let add = Op::Add {
            comment: Box::new(Comment {
                path: "a.rs".to_owned(),
                text: "Once".to_owned(),
                ..Comment::default()
            }),
            reply_to: None,
        };
        let (written, ids) = comments.apply(vec![add]).expect("written");
        assert!(ids[0].is_some(), "{ids:?}");
        let unread = written.unread.unwrap_or_default();
        assert!(unread.contains("local.json"), "{unread}");
        let summary = comments.keep_summary("Two nits.").expect("kept");
        assert!(summary.unread.is_some());
        let file = review::read(&path).expect("reads");
        assert_eq!(file.comments.len(), 1, "written once");
        assert_eq!(file.summary.as_deref(), Some("Two nits."));
    }

    /// While new threads go to the local review's file, someone else's, as
    /// one added through the control API, goes to the comments file: the
    /// local one's drafts shown are yours only. It shows there as theirs,
    /// not posted.
    #[test]
    fn someone_elses_new_thread_goes_where_it_shows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, local) = (dir.path().join("7.json"), dir.path().join("local.json"));
        let mut comments = Comments::new(path, Some(local.clone()), true, None);
        comments.set_new_local(true);
        let theirs = Op::Add {
            comment: Box::new(Comment {
                path: "a.rs".to_owned(),
                text: "An agent's".to_owned(),
                author: "dffy ctl".to_owned(),
                ..Comment::default()
            }),
            reply_to: None,
        };
        comments.apply(vec![theirs]).expect("written");
        assert_eq!(review::read(&local).expect("reads").comments.len(), 0);
        assert_eq!(
            (comments.review.comments.iter())
                .find(|c| c.text == "An agent's")
                .map(|c| comments.standing(c)),
            Some(Standing::Unposted)
        );
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

    /// On a review session's diff, a thread places on the lines its anchor
    /// finds, a draft in it makes it a draft, and one whose lines are gone
    /// shows where it was with its code; a local draft that finds no line
    /// isn't shown, nor counted, and the rest are.
    #[test]
    fn threads_place_by_their_anchors_and_merge_the_local_drafts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, local) = (dir.path().join("7.json"), dir.path().join("local.json"));
        let anchored = |id: &str, line: usize, text: &str, rest: &str| {
            format!(
                r#"{{"id":"{id}","path":"a.rs","side":"new","start":{{"old":null,"new":{line}}},"end":{{"old":null,"new":{line}}},"text":"x","created":"2026-11-17T10:00:00Z","anchor":{{"before":[],"lines":["{text}"],"after":[]}}{rest}}}"#
            )
        };
        let file =
            |comments: &[String]| format!(r#"{{"version":1,"comments":[{}]}}"#, comments.join(","));
        std::fs::write(
            &path,
            file(&[
                anchored("p1", 2, "two", r#","author":"ana","ref":"h:1""#),
                r#"{"id":"r1","path":"a.rs","text":"Yes","author":"","created":"2026-11-17T11:00:00Z","in_reply_to":"p1"}"#.to_owned(),
                anchored("p2", 8, "gone", r#","author":"bo","ref":"h:2""#),
            ]),
        )
        .expect("write");
        std::fs::write(
            &local,
            file(&[
                anchored("l1", 3, "three", r#","author":"""#),
                anchored("l2", 3, "nowhere", r#","author":"""#),
            ]),
        )
        .expect("write");
        let mut comments = Comments::new(path, Some(local), true, None);
        assert_eq!(comments.reload(), Ok(Some(vec![BString::from("a.rs")])));
        let annotated = comments.annotated(b"a.rs").expect("commented");
        let landed = land(&annotated, &new_file("one\ntwo\nthree\n"), false);
        assert!(comments.place(b"a.rs", 1, landed).is_some(), "current");
        let threads = comments.threads(b"a.rs").expect("placed");
        let at = |t: &Thread| (t.root.clone(), t.at.clone(), t.look, t.local);
        let line = |lines| At::Line {
            side: Side::New,
            lines,
        };
        assert_eq!(
            threads.iter().map(at).collect::<Vec<_>>(),
            [
                ("p1".to_owned(), line(1..2), Look::Draft, false),
                ("l1".to_owned(), line(2..3), Look::Draft, true),
                ("p2".to_owned(), At::Lost, Look::Open, false),
            ]
        );
        assert_eq!(
            threads[0]
                .comments
                .iter()
                .map(|n| n.standing)
                .collect::<Vec<_>>(),
            [Standing::Posted, Standing::Draft]
        );
        let snippet = threads[2].snippet.as_ref().expect("its code");
        assert_eq!(snippet.heading, "line 8 not found");
        assert_eq!(comments.counts(), (3, 2), "l2 is out of view");
        assert_eq!(comments.local_drafts(), 1);
    }

    /// A lost thread's code is highlighted where it's placed, off the
    /// thread that makes its threads: making them again parses nothing.
    #[test]
    fn lost_code_is_highlighted_where_it_is_placed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("comments.json");
        std::fs::write(
            &path,
            r#"{"version":1,"comments":[{"id":"g","path":"a.rs","side":"new","start":{"old":null,"new":9},"end":{"old":null,"new":9},"text":"x","created":"2026-11-17T10:00:00Z","anchor":{"before":[],"lines":["fn gone() {}"],"after":[]}}]}"#,
        )
        .expect("write");
        let mut comments = Comments::new(path, None, false, None);
        comments.set_highlight(true);
        comments.reload().expect("read");
        let annotated = comments.annotated(b"a.rs").expect("commented");
        let landed = std::thread::spawn(move || land(&annotated, &new_file("one\n"), true))
            .join()
            .expect("placed");
        let before = painted_count();
        assert!(comments.place(b"a.rs", 1, landed).is_some());
        let threads = comments.threads(b"a.rs").expect("placed");
        let code = &threads[0].snippet.as_ref().expect("its code").rows[0];
        assert!(code.pieces.iter().any(|(s, _)| s.is_some()), "{code:?}");
        assert_eq!(painted_count(), before);
    }

    /// `a.rs`, added with `new`.
    fn new_file(new: &str) -> FileDiff {
        let (old, new) = (text(""), text(new));
        FileDiff {
            change: FileChange {
                path: "a.rs".into(),
                status: Status::Added,
                old: None,
                new: None,
                staging: None,
            },
            hunks: hunks(&old, &new, &Options::default()),
            old,
            new,
            binary: false,
            too_large: false,
            note: None,
        }
    }

    /// `change` diffed from `old` to `new`.
    fn diffed(change: FileChange, old: &str, new: &str) -> FileDiff {
        let (old, new) = (text(old), text(new));
        FileDiff {
            change,
            hunks: hunks(&old, &new, &Options::default()),
            old,
            new,
            binary: false,
            too_large: false,
            note: None,
        }
    }

    fn change(path: &str, status: Status, sides: (bool, bool)) -> FileChange {
        let side = |on: bool| {
            on.then_some(dffy_core::git::Side::Worktree {
                mode: dffy_core::git::Mode::File,
                id: None,
            })
        };
        FileChange {
            path: path.into(),
            status,
            old: side(sides.0),
            new: side(sides.1),
            staging: None,
        }
    }

    /// A snapshot of the file places each file's threads on its diff, in
    /// thread order with their replies: on their lines, on the whole file,
    /// or lost; only the branch's threads and those on none.
    #[test]
    fn a_snapshot_places_each_files_threads_on_its_diff() {
        let review = Review::parse(
            br#"{"version":1,"comments":[
{"id":"g","path":"a.rs","side":"new","start":{"old":null,"new":9},"end":{"old":null,"new":9},"text":"Gone","author":"","created":"t","anchor":{"before":[],"lines":["nine"],"after":[]}},
{"id":"n","path":"a.rs","side":"new","start":{"old":null,"new":5},"end":{"old":null,"new":5},"text":"Two","author":"ana","created":"t","anchor":{"before":[],"lines":["two"],"after":[]}},
{"id":"r","path":"a.rs","text":"Yes","author":"","created":"t","in_reply_to":"n"},
{"id":"f","path":"a.rs","text":"File","author":"","created":"t"},
{"id":"o","path":"a.rs","text":"Elsewhere","author":"","created":"t","branch":"other"},
{"id":"b","path":"b.rs","text":"B","author":"","created":"t"}
]}"#,
        )
        .expect("parses");
        let snapshot = Snapshot::new(&review, review::Branches::On(Some("main")));
        let a = change("a.rs", Status::Modified, (true, true));
        let diff = diffed(a.clone(), "one\nthree\n", "one\ntwo\nthree\n");
        let placed: Vec<(Vec<&str>, At)> = (snapshot.on(&a, &diff, false).into_iter())
            .map(|h| (h.thread.iter().map(|c| c.id.as_str()).collect(), h.at))
            .collect();
        let line = |side, lines| At::Line { side, lines };
        assert_eq!(
            placed,
            [
                (vec!["f"], At::File),
                (vec!["n", "r"], line(Side::New, 1..2)),
                (vec!["g"], At::Lost),
            ]
        );
        assert!(snapshot.has(&a, false));
        let c = change("c.rs", Status::Modified, (true, true));
        assert!(!snapshot.has(&c, false));
        assert!(
            snapshot
                .on(&c, &diffed(c.clone(), "x\n", "y\n"), false)
                .is_empty()
        );

        // With -R the change and its diff come reversed: the new side the
        // thread is on is the old side shown.
        let reversed = a.clone().reversed();
        let back = diffed(reversed.clone(), "one\ntwo\nthree\n", "one\nthree\n");
        let at: Vec<At> = (snapshot.on(&reversed, &back, true).into_iter())
            .map(|h| h.at)
            .collect();
        assert_eq!(at, [At::File, line(Side::Old, 1..2), At::Lost]);
    }

    /// A thread on a renamed file's old side shows on the rename when the
    /// diff pairs it, and on the deletion of its old path when it doesn't,
    /// never on both.
    #[test]
    fn an_old_side_thread_shows_once_whether_the_rename_is_paired_or_not() {
        let review = Review::parse(
            br#"{"version":1,"comments":[
{"id":"o","path":"new.rs","old_path":"old.rs","side":"old","start":{"old":2,"new":null},"end":{"old":2,"new":null},"text":"Old","author":"","created":"t","anchor":{"before":[],"lines":["dos"],"after":[]}}
]}"#,
        )
        .expect("parses");
        let snapshot = Snapshot::new(&review, review::Branches::All);
        let ids = |change: &FileChange, old: &str, new: &str| -> Vec<At> {
            let diff = diffed(change.clone(), old, new);
            (snapshot.on(change, &diff, false).into_iter())
                .map(|h| h.at)
                .collect()
        };
        let old_line = At::Line {
            side: Side::Old,
            lines: 1..2,
        };
        let renamed = change(
            "new.rs",
            Status::Renamed {
                from: "old.rs".into(),
                similarity: 90,
            },
            (true, true),
        );
        assert_eq!(
            ids(&renamed, "uno\ndos\n", "uno\ndos\ntres\n"),
            std::slice::from_ref(&old_line)
        );
        let added = change("new.rs", Status::Added, (false, true));
        assert_eq!(ids(&added, "", "uno\ndos\ntres\n"), []);
        let deleted = change("old.rs", Status::Deleted, (true, false));
        assert_eq!(ids(&deleted, "uno\ndos\n", ""), [old_line]);
    }

    /// A comment's text as plain text for a box: paragraphs a blank line
    /// apart, list items marked, code kept as written, a link's address
    /// after its text, HTML comments left out.
    #[test]
    fn markdown_reads_as_plain_text() {
        let text = "Use **checked** math.\nSee [docs](https://x.y).\n\n- [x] one\n- two\n\n1. first\n\n```rust\nlet a = 1;\n    b\n```\n\n> quoted\n<!-- hidden -->";
        assert_eq!(
            plain_text(text),
            [
                "Use checked math.",
                "See docs <https://x.y>.",
                "",
                "- ☑ one",
                "- two",
                "",
                "1. first",
                "",
                "    let a = 1;",
                "        b",
                "",
                "> quoted",
            ]
        );
        assert_eq!(plain_text("a\u{1b}[31m"), ["a^[[31m"]);
    }

    /// A comment's text changing makes its threads again at once, from
    /// where they were placed; a new comment on lines shows once it's
    /// placed, and placements made before it came are refused.
    #[test]
    fn threads_are_made_again_at_once_and_new_lines_wait_for_their_placement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("comments.json");
        let comment = |id: &str, line: usize, anchored: &str, text: &str| {
            format!(
                r#"{{"id":"{id}","path":"a.rs","side":"new","start":{{"old":null,"new":{line}}},"end":{{"old":null,"new":{line}}},"text":"{text}","created":"2026-11-17T10:00:00Z","anchor":{{"before":[],"lines":["{anchored}"],"after":[]}}}}"#
            )
        };
        let write = |comments: &[String]| {
            let json = format!(r#"{{"version":1,"comments":[{}]}}"#, comments.join(","));
            std::fs::write(&path, json).expect("write");
        };
        let diff = new_file("one\ntwo\nthree\n");
        let shown = |comments: &Comments| -> Vec<(String, String)> {
            let threads = comments.threads(b"a.rs").expect("placed");
            (threads.iter())
                .map(|t| (t.root.clone(), t.comments[0].text.clone()))
                .collect()
        };
        write(&[comment("a", 2, "two", "First")]);
        let mut comments = Comments::new(path.clone(), None, false, None);
        comments.reload().expect("read");
        let first = comments.annotated(b"a.rs").expect("commented");
        let landed = land(&first, &diff, false);
        assert!(comments.place(b"a.rs", 1, landed.clone()).is_some());

        write(&[comment("a", 2, "two", "Edited")]);
        comments.reload().expect("read");
        assert_eq!(shown(&comments), [("a".to_owned(), "Edited".to_owned())]);
        let same = comments.annotated(b"a.rs").expect("commented");
        assert_eq!(same.generation, first.generation, "the lines didn't move");

        write(&[
            comment("a", 2, "two", "Edited"),
            comment("b", 3, "three", "New"),
        ]);
        comments.reload().expect("read");
        assert_eq!(shown(&comments), [("a".to_owned(), "Edited".to_owned())]);
        assert!(
            comments.place(b"a.rs", 1, landed).is_none(),
            "placed before b came"
        );
        let now = comments.annotated(b"a.rs").expect("commented");
        assert!(
            comments
                .place(b"a.rs", 1, land(&now, &diff, false))
                .is_some()
        );
        assert_eq!(
            shown(&comments),
            [
                ("a".to_owned(), "Edited".to_owned()),
                ("b".to_owned(), "New".to_owned())
            ]
        );
    }
}

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

pub(crate) mod compose;
mod drafts;
pub(crate) mod reload;
pub(crate) mod snapshot;
pub(crate) mod snippet;
pub(crate) mod text;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dffy_core::annotation::{Annotated, Annotation, AnnotationSet, Placement};
use dffy_core::bstr::BString;
use dffy_core::document::{self, File, Side};
use dffy_core::git::{FileDiff, Status};
use dffy_core::view::{Spans, Syntax};

use self::reload::{Stamp, Stat};
use self::snippet::{paint_lost, snippet};
use self::text::{epoch, summary};
use crate::review::{self, Comment, Review};

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
    /// `P` leaves it, since it would post under your name. Or yours, kept
    /// local, as a mention that asked an agent.
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
    /// Words after its author, as given: how an agent's reply was made,
    /// `read-only sandbox`. A general comment's are its thread's, in
    /// [`At::Conversation`].
    pub label: Option<String>,
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
    /// Whether it holds a comment that posting leaves, as an agent's, that
    /// `A` makes yours.
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

    /// Where `c` stands, a reply a draft only as [`crate::review::Review::drafts`]
    /// has it.
    fn standing(&self, c: &Comment) -> Standing {
        let root = c.in_reply_to.as_deref().and_then(|r| self.find(r));
        match (&c.reference, self.session) {
            (Some(_), _) => Standing::Posted,
            (None, false) => Standing::Kept,
            (None, true) if c.draft() && root.is_none_or(Comment::takes_replies) => Standing::Draft,
            (None, true) => Standing::Unposted,
        }
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

    /// The path whose entry shows the thread with root `root` (see
    /// [`Comments::homes`]); empty for a general comment's.
    pub(crate) fn home_of(&self, root: &str) -> Option<&str> {
        self.find(root).map(|c| self.home(c))
    }

    /// The file the thread with root `root` is in, and whether that's a
    /// review session's.
    pub(crate) fn file_with(&self, root: &str) -> (&Path, bool) {
        match &self.local {
            Some(local) if self.from_local.contains(root) || self.others_local.contains(root) => {
                (local, false)
            }
            _ => (&self.path, self.session),
        }
    }

    /// Whether `root` starts a thread on another branch.
    pub(crate) fn elsewhere(&self, root: &str) -> bool {
        self.others.iter().any(|c| c.id == root)
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
                    label: c.label.clone().filter(|_| !c.path.is_empty()),
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

#[cfg(test)]
mod tests {
    use dffy_core::diff::{Options, Text, hunks};
    use dffy_core::git::FileChange;

    use super::snippet::painted_count;
    use super::*;

    pub(super) fn text(s: &str) -> Text {
        Text::new(s.as_bytes().to_vec())
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

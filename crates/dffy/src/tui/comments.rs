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
use std::path::PathBuf;
use std::sync::Arc;

use dffy_core::bstr::BString;
use dffy_core::diff::{Hunk, LineKind, Text};
use dffy_core::document::{File, Side};
use dffy_core::git::{FileChange, FileDiff, Status};
use dffy_core::view::Attributes;
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
    Posted,
    Draft,
}

impl Look {
    /// Shapes differ as well as colors: without color the theme is
    /// attributes only.
    pub(crate) fn glyph(self) -> &'static str {
        match self {
            Self::Resolved => "○",
            Self::Posted => "●",
            Self::Draft => "◆",
        }
    }

    pub(crate) fn role(self) -> Role {
        match self {
            Self::Resolved => Role::Dim,
            Self::Posted => Role::Hint,
            Self::Draft => Role::Warn,
        }
    }
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
    /// Not posted: its text can change.
    pub draft: bool,
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

pub(super) struct Comments {
    path: PathBuf,
    review: Review,
    /// Each path's comments, replies included, as indices into `review`.
    by_path: HashMap<BString, Vec<usize>>,
    placed: HashMap<BString, Placed>,
    /// Where each commented path is in the list, when it is.
    located: HashMap<BString, usize>,
}

impl Comments {
    /// No comments until [`Comments::reload`] reads `path`.
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            path,
            review: Review::default(),
            by_path: HashMap::new(),
            placed: HashMap::new(),
            located: HashMap::new(),
        }
    }

    pub(super) fn notice(&self) -> Option<&str> {
        self.review.notice.as_deref()
    }

    /// Reads the file again. Returns the paths whose comments changed,
    /// `None` when nothing did; on an error the last read stays.
    pub(super) fn reload(&mut self) -> Result<Option<Vec<BString>>, String> {
        let review = review::read(&self.path)?;
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

    /// Applies `ops` to the file under its lock, then reads it back.
    /// Returns the paths whose comments changed and the id each operation
    /// touched.
    pub(super) fn apply(
        &mut self,
        ops: Vec<Op>,
    ) -> Result<(Vec<BString>, Vec<Option<String>>), String> {
        let ids = review::apply(&self.path, ops)?;
        Ok((self.reload()?.unwrap_or_default(), ids))
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

    /// The files in the list with comments, in list order.
    pub(super) fn files(&self) -> Vec<usize> {
        let mut out: Vec<usize> = self.located.values().copied().collect();
        out.sort_unstable();
        out
    }

    /// Every path with comments.
    pub(super) fn paths(&self) -> HashSet<BString> {
        self.by_path.keys().cloned().collect()
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
    /// line.
    pub(super) fn place(&mut self, path: &[u8], version: u64, texts: Option<[&Text; 2]>) {
        let Some(indices) = self.by_path.get(path) else {
            return;
        };
        let mut threads = Vec::new();
        let mut unplaced = Vec::new();
        let roots = review::threads(indices.iter().map(|&i| &self.review.comments[i]));
        for comment in roots.into_iter().filter(|c| c.in_reply_to.is_none()) {
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
                    } else {
                        unplaced.push(comment.id.clone());
                        At::Lost
                    }
                }
            };
            threads.push(Thread {
                root: comment.id.clone(),
                at,
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
                        draft: c.reference.is_none(),
                    })
                    .collect(),
                lines: comment
                    .anchor
                    .as_ref()
                    .map(|a| a.lines.clone())
                    .unwrap_or_default(),
            });
        }
        self.placed.insert(
            path.into(),
            Placed {
                version,
                threads: threads.into(),
                unplaced,
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
        let indices = self.by_path.get(path)?;
        let look = indices
            .iter()
            .map(|&i| &self.review.comments[i])
            .filter(|c| c.in_reply_to.is_none())
            .map(|c| self.look(c))
            .max()
            .unwrap_or(Look::Posted);
        Some((indices.len(), look))
    }

    /// Threads in the whole file, and comments not posted yet.
    pub(super) fn counts(&self) -> (usize, usize) {
        let comments = &self.review.comments;
        (
            comments.iter().filter(|c| c.in_reply_to.is_none()).count(),
            comments.iter().filter(|c| c.reference.is_none()).count(),
        )
    }

    /// Drafts to post: new threads, and replies.
    pub(super) fn unsent(&self) -> (usize, usize) {
        let drafts = self
            .review
            .comments
            .iter()
            .filter(|c| c.reference.is_none());
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
            .any(|c| c.reference.is_none())
        {
            Look::Draft
        } else {
            Look::Posted
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
            if root.range().is_none() {
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
    let utf8 = |path: &[u8]| {
        std::str::from_utf8(path)
            .map(str::to_owned)
            .map_err(|_| "the comments file holds UTF-8 paths only; this one isn't")
    };
    let (text, ours) = match side {
        Side::Old => (&diff.old, review::Side::Old),
        Side::New => (&diff.new, review::Side::New),
    };
    if lines.is_empty() || lines.end > text.len() {
        return Err("no line of code here to comment on");
    }
    let old_path = match &change.status {
        Status::Renamed { from, .. } => Some(utf8(from)?),
        _ => None,
    };
    Ok(Comment {
        path: utf8(&change.path)?,
        old_path,
        side: ours,
        start: Some(numbers(&diff.hunks, side, lines.start)),
        end: Some(numbers(&diff.hunks, side, lines.end - 1)),
        anchor: Some(Anchor::new(text, lines.clone())),
        distance: review::distance(lines, ours, &diff.hunks),
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
    /// The ends of a folded thread's one row.
    ends: [&'static str; 2],
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
        if focused {
            Self {
                role,
                corners: ["┏", "┓", "┗", "┛"],
                across,
                down,
                ends: ["╺", "╸"],
            }
        } else {
            Self {
                role,
                corners: ["┌", "┐", "└", "┘"],
                across,
                down,
                ends: ["╶", "╴"],
            }
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

    /// A row of the border between `ends`, carrying `left` after the first
    /// and `right` before the last when it has room for them.
    fn edge(&mut self, ends: [&str; 2], left: &[(Ink, &str)], right: &[(Role, &str)]) {
        let Border { role, across, .. } = self.border;
        let room = self.width.saturating_sub(2);
        let right_width = spans_width(right) + 2;
        // The left side's text goes first; the right's only beside a
        // dozen columns of it.
        let right = if right.is_empty() || right_width + 12 > room {
            &[][..]
        } else {
            right
        };
        let right_width = if right.is_empty() { 0 } else { right_width };
        self.c.push(role, ends[0]);
        let mut used = right_width;
        if !left.is_empty() && room >= used + 3 {
            self.c.push(Role::Plain, " ");
            used += 2 + cut(self.c, left, room - used - 2);
            self.c.push(Role::Plain, " ");
        }
        self.c.push(role, &across.repeat(room.saturating_sub(used)));
        if !right.is_empty() {
            self.c.push(Role::Plain, " ");
            cut(self.c, right, right_width - 2);
            self.c.push(Role::Plain, " ");
        }
        self.c.push(role, ends[1]);
        self.c.end_line();
    }

    fn top(&mut self, spans: &[(Ink, &str)]) {
        let [left, right, ..] = self.border.corners;
        self.edge([left, right], spans, &[]);
    }

    /// A reply's heading, under a blank row parting it from the comment
    /// above.
    fn heading(&mut self, spans: &[(Ink, &str)]) {
        self.body::<Role>(&[]);
        self.body(spans);
    }

    fn bottom(&mut self) {
        let [.., left, right] = self.border.corners;
        self.edge([left, right], &[], &[]);
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

/// The words after a thread's first author: its state, then where it is
/// when that's not on a line.
fn tags(thread: &Thread, first: &Note) -> Vec<(Role, &'static str)> {
    let mut out = Vec::new();
    if first.draft {
        out.push((Role::Warn, "draft"));
    }
    if thread.resolved {
        out.push((Role::Dim, "resolved"));
    }
    if thread.outdated {
        out.push((Role::Warn, "outdated"));
    }
    match thread.at {
        At::File => out.push((Role::Dim, "on the file")),
        At::Lost => out.push((Role::Warn, "lines not found")),
        At::Line { .. } => {}
    }
    out
}

/// Writes `thread`'s box, `width` columns wide: the first comment's
/// author, age and state in the top border, each reply's on a row of its
/// own after a blank one, every comment's text wrapped inside, and a
/// suggestion as the lines it replaces and its own. Authors are bold.
/// Collapsed, one row of border sums it up.
/// `focused` draws the border heavy. `writing`, when the text box is in
/// this thread, takes the place of the comment it edits or follows the
/// last as a reply. `now` is seconds since 1970.
pub(crate) fn thread_rows(
    thread: &Thread,
    (collapsed, focused): (bool, bool),
    mut writing: Option<&mut Writing>,
    (width, now): (usize, u64),
    c: &mut Canvas,
) {
    let Some(first) = thread.notes.first() else {
        return;
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
        let right = if count.is_empty() {
            Vec::new()
        } else {
            vec![(Role::Dim, count.as_str())]
        };
        frame.edge(border.ends, &spans, &right);
        return;
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
        } else if note.draft {
            vec![(Role::Warn, "draft")]
        } else {
            Vec::new()
        };
        for (role, word) in words {
            spans.extend([(Role::Dim.into(), " · "), (role.into(), word)]);
        }
        let editing = writing
            .as_deref_mut()
            .filter(|w| matches!(&w.at, Writes::Edit { id, .. } if *id == note.id));
        if editing.is_some() {
            spans.push((Role::Dim.into(), " · esc saves · ctrl+e editor"));
        }
        if i == 0 {
            frame.top(&spans);
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
            (Role::Dim.into(), " · reply · esc saves · ctrl+e editor"),
        ]);
        frame.text_box(&mut writing.text);
    }
    frame.bottom();
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
            " · new comment · esc saves, empty discards · ctrl+e editor",
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
        let lines = list_lines(&list, waiting, right - x);
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

fn list_lines(list: &[(&Comment, Option<Why>)], waiting: usize, width: usize) -> Vec<Line> {
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
        let state = if root.resolved {
            "resolved"
        } else if root.reference.is_none() {
            "draft"
        } else {
            "posted"
        };
        let note = match why {
            Some(Why::File) => " · on the file",
            Some(Why::Absent) => " · file not in this diff",
            Some(Why::Moved) => " · lines not found",
            None => "",
        };
        let first = root.text.lines().next().unwrap_or_default();
        let text = format!(
            "{} · {state}{note} · {}",
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
}

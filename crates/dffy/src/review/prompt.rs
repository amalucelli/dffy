//! A thread or a selection as a prompt a coding agent can answer: the
//! thread's comments, where it is, the lines it's on with some around them,
//! and the file's changes near them. Copy as prompt, `dffy comments
//! prompt` and MCP's `thread_prompt` give it as made here. An agent a
//! thread is handed to starts with only a pointer to it ([`handoff`]).
//!
//! Plain text with Markdown fences, the same for the same input: no times,
//! nothing of the viewer's state. The agent may have nothing else to read,
//! so every cut is marked where it was made.

use std::fmt::Write as _;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use dffy_core::annotation::Anchor;
use dffy_core::diff::{Hunk, LineKind, Text};
use dffy_core::document::Side;
use dffy_core::git::FileDiff;

use super::model::At;
use super::{Comment, fence, who};

/// A ceiling on a prompt's size, in bytes. Over it, a prompt is trimmed,
/// first to last: the file's other hunks, farthest from the lines first;
/// the lines of the hunks on them beyond [`AROUND`] of them; the lines
/// around, down to [`LEAST_AROUND`]; the replies between the first comment
/// and the last two, the earliest first, but those with a suggestion; then
/// the commented lines themselves, to their first and last few. What it's
/// about, the first comment, the last two, the suggestions and the closing
/// ask are never cut, so a thread longer than this makes a longer prompt.
pub(crate) const MAX_BYTES: usize = 32 << 10;

/// Lines shown on each side of the commented or selected ones.
const AROUND: usize = 5;
const LEAST_AROUND: usize = 3;

/// A prompt, and whether trimming cut something to fit [`MAX_BYTES`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prompt {
    pub text: String,
    pub trimmed: bool,
}

/// The thread of `comments`, its first comment first, shown `at` on
/// `diff`, its file's line diff; without one, its lines as written.
/// `elsewhere` when HEAD isn't the head of the review the thread is on, so
/// the files on disk aren't the version the thread is about: the prompt
/// says so, for an agent that reads them.
pub(crate) fn thread(
    comments: &[&Comment],
    at: &At,
    diff: Option<&FileDiff>,
    elsewhere: bool,
) -> Prompt {
    thread_within(elsewhere, comments, at, diff, MAX_BYTES)
}

/// Lines `lines` (0-based) of `side` of `diff`, a line diff, selected.
pub(crate) fn selection(diff: &FileDiff, side: Side, lines: Range<usize>) -> Prompt {
    selection_within(diff, side, lines, MAX_BYTES)
}

/// Where `dffy comments` finds a thread handed to an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kept<'a> {
    /// The local review's file, which it reads by default.
    Local,
    /// The review session's, which `--session` reads.
    Session,
    /// A file `--comments` names.
    File(&'a Path),
}

/// What the user's own coding agent starts with to work on thread `id`:
/// where to read it, never its text, which may be anyone's. It's to change
/// what the thread asks only, and say what it changed there.
pub(crate) fn handoff(id: &str, kept: Kept) -> String {
    let read = match kept {
        Kept::Local => {
            format!("run dffy comments prompt {id} (or the thread_prompt MCP tool)")
        }
        Kept::Session => format!(
            "run dffy comments --session prompt {id} (or the thread_prompt MCP tool with \
             session: true)"
        ),
        Kept::File(path) => {
            let file = crate::terminal::quote(path.as_os_str().as_bytes());
            format!(
                "run dffy --comments {} comments prompt {id}",
                String::from_utf8_lossy(&file)
            )
        }
    };
    format!(
        "Address dffy review thread {id}: {read}, change only what the thread asks, then reply \
         in the thread with what you changed."
    )
}

/// What an agent handed every open thread of yours starts with: where to
/// read them, as [`handoff`] says for one.
pub(crate) fn open_handoff(kept: Kept) -> String {
    let comments = match kept {
        Kept::Local => "dffy comments".to_owned(),
        Kept::Session => "dffy comments --session".to_owned(),
        Kept::File(path) => {
            let file = crate::terminal::quote(path.as_os_str().as_bytes());
            format!(
                "dffy --comments {} comments",
                String::from_utf8_lossy(&file)
            )
        }
    };
    format!(
        "Address the open dffy review threads: run {comments} prompt --open, change only what \
         each thread asks, reply in each thread with what you changed ({comments} apply --as \
         <you>)."
    )
}

/// A thread for [`open_threads`]: its comments, its first comment first,
/// where it's shown, and its file's line diff when there's one.
pub(crate) type Open<'a> = (&'a [&'a Comment], &'a At, Option<&'a FileDiff>);

/// Every thread of `threads` in one prompt, each under its first comment's
/// id as [`thread`] gives it, then one ask for them all; `others`, threads
/// left out as on other branches, are counted. `elsewhere` as for
/// [`thread`], said once. The cap grows with the threads, [`MAX_BYTES`]
/// for each of the first 4, and each thread is trimmed alone to its share
/// of it; as for one thread, what's never cut can make a longer prompt.
pub(crate) fn open_threads(threads: &[Open], others: usize, elsewhere: bool) -> Prompt {
    let n = threads.len();
    let mut text = if n == 0 {
        String::from("No open review threads of yours.\n\n")
    } else {
        format!(
            "{} of yours, each under its id.\n\n",
            count(n, "open review thread")
        )
    };
    if elsewhere && n > 0 {
        text.push_str(
            "The files on disk aren't this review's version (HEAD isn't its head); rely on the \
             hunks quoted.\n\n",
        );
    }
    let share = MAX_BYTES * n.min(4) / n.max(1);
    let mut trimmed = false;
    for &(comments, at, diff) in threads {
        let Some(root) = comments.first() else {
            continue;
        };
        let _ = write!(text, "## Thread {}\n\n", root.id);
        let mut parts = thread_parts(false, comments, at, diff);
        parts.tail = String::new();
        let one = parts.fit(share);
        trimmed |= one.trimmed;
        text.push_str(&one.text);
    }
    if others > 0 {
        let s = if others == 1 { "" } else { "s" };
        let _ = write!(
            text,
            "[{others} open thread{s} of yours on other branches left out]\n\n"
        );
    }
    if n > 0 {
        text.push_str(
            "For each thread, change only what its last comment asks, then reply in that thread, \
             by its id, with what you changed.\n",
        );
    }
    Prompt { text, trimmed }
}

fn thread_within(
    elsewhere: bool,
    comments: &[&Comment],
    at: &At,
    diff: Option<&FileDiff>,
    cap: usize,
) -> Prompt {
    if comments.is_empty() {
        return Prompt {
            text: String::new(),
            trimmed: false,
        };
    }
    thread_parts(elsewhere, comments, at, diff).fit(cap)
}

/// [`thread_within`]'s parts, before trimming; `comments` isn't empty.
fn thread_parts<'a>(
    elsewhere: bool,
    comments: &'a [&'a Comment],
    at: &At,
    diff: Option<&FileDiff>,
) -> Parts<'a> {
    let root = comments[0];
    let mut head = String::from("A review thread on ");
    let mut code = None;
    let mut anchor = None;
    let mut hunks = Vec::new();
    match (at, diff) {
        (At::Conversation { .. }, _) => head.push_str("the review as a whole"),
        (At::File, _) => {
            let _ = write!(head, "the whole file {}", root.path);
            hunks = diff.map(|d| all_hunks(d, None)).unwrap_or_default();
        }
        (At::Line { side, lines }, Some(diff)) => {
            place(&mut head, &root.path, *side, lines);
            code = Some(Code::from_text(side_text(diff, *side), lines.clone()));
            anchor = Some((*side, lines.clone()));
            hunks = all_hunks(diff, anchor.as_ref());
        }
        (At::Line { .. } | At::Lost, _) => {
            match root.range() {
                Some((side, lines)) => {
                    place(&mut head, &root.path, side_of(side), &lines);
                    head.push_str(if diff.is_some() {
                        ", as written: the diff has these lines nowhere now"
                    } else {
                        ", as written"
                    });
                    code = root
                        .anchor
                        .as_ref()
                        .map(|a| Code::from_anchor(a, lines.start));
                }
                None => head.push_str(&root.path),
            }
            hunks = diff.map(|d| all_hunks(d, None)).unwrap_or_default();
        }
    }
    if root.resolved {
        head.push_str(", resolved");
    }
    if root.outdated {
        head.push_str(", outdated on its host");
    }
    head.push_str(".\n\n");
    if elsewhere {
        head.push_str(
            "The files on disk aren't this review's version (HEAD isn't its head); rely on the \
             hunks quoted.\n\n",
        );
    }
    let mut tail = String::from("Answer the thread's last comment.");
    if code.is_some() {
        tail.push_str(
            " To propose a change to the commented lines, end with the lines that replace \
             them in a fenced block opened with ```suggestion.",
        );
    }
    tail.push('\n');
    Parts {
        head,
        comments,
        code: code.map(|c| {
            (
                "The commented lines, marked >, and the lines around them:",
                c,
            )
        }),
        anchor,
        hunks,
        tail,
    }
}

fn selection_within(diff: &FileDiff, side: Side, lines: Range<usize>, cap: usize) -> Prompt {
    let path = match side {
        Side::Old => diff.change.old_path(),
        Side::New => diff.change.path.as_ref(),
    };
    let mut head = String::from("Lines selected in a diff: ");
    place(&mut head, &String::from_utf8_lossy(path), side, &lines);
    head.push_str(".\n\n");
    let anchor = Some((side, lines.clone()));
    Parts {
        head,
        comments: &[],
        code: Some((
            "The selected lines, marked >, and the lines around them:",
            Code::from_text(side_text(diff, side), lines),
        )),
        hunks: all_hunks(diff, anchor.as_ref()),
        anchor,
        tail: String::new(),
    }
    .fit(cap)
}

/// `path:12-14 (the new version of the file)`, from 0-based `lines`.
fn place(out: &mut String, path: &str, side: Side, lines: &Range<usize>) {
    let (first, last) = (lines.start + 1, lines.end.max(lines.start + 1));
    let _ = if first == last {
        write!(out, "{path}:{first}")
    } else {
        write!(out, "{path}:{first}-{last}")
    };
    out.push_str(match side {
        Side::Old => " (the old version of the file, before the change)",
        Side::New => " (the new version of the file)",
    });
}

fn side_of(side: super::Side) -> Side {
    match side {
        super::Side::Old => Side::Old,
        super::Side::New => Side::New,
    }
}

fn side_text(diff: &FileDiff, side: Side) -> &Text {
    match side {
        Side::Old => &diff.old,
        Side::New => &diff.new,
    }
}

/// Line `i` of `text` without its line break, invalid UTF-8 replaced.
fn line(text: &Text, i: usize) -> String {
    String::from_utf8_lossy(text.line(i))
        .trim_end_matches(['\n', '\r'])
        .to_owned()
}

/// `n` and `word`, plural unless one.
fn count(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// The marker standing for `n` lines cut.
fn cut_lines(n: usize) -> String {
    format!("[… {} …]", count(n, "line"))
}

/// Numbered lines of code: the marked ones, and those around them by how
/// far they are from the nearest marked one.
struct Code {
    /// 1-based number, how far from the marked lines (0 for one of them),
    /// text.
    lines: Vec<(usize, usize, String)>,
}

impl Code {
    /// Lines `marked` of `text`, with [`AROUND`] lines on each side.
    fn from_text(text: &Text, marked: Range<usize>) -> Self {
        let end = marked.end.min(text.len());
        let start = marked.start.min(end);
        let lines = (start.saturating_sub(AROUND)..(end + AROUND).min(text.len()))
            .map(|i| {
                let far = if i < start {
                    start - i
                } else {
                    (i + 1).saturating_sub(end)
                };
                (i + 1, far, line(text, i))
            })
            .collect();
        Self { lines }
    }

    /// An anchor's lines as written, the first numbered `first + 1`.
    fn from_anchor(anchor: &Anchor, first: usize) -> Self {
        let before = anchor.before.len();
        let number = |i: usize| (first + 1 + i).saturating_sub(before);
        let marked = before + anchor.lines.len();
        let lines = (anchor
            .before
            .iter()
            .chain(&anchor.lines)
            .chain(&anchor.after))
        .enumerate()
        .map(|(i, text)| {
            let far = if i < before {
                before - i
            } else {
                (i + 1).saturating_sub(marked)
            };
            (number(i), far, text.clone())
        })
        .collect();
        Self { lines }
    }

    /// The lines within `around` of the marked ones, in a code block; with
    /// `cut`, the marked ones only as their first and last `cut`.
    fn render(&self, around: usize, cut: Option<usize>, out: &mut String) {
        let marked = self.lines.iter().filter(|(_, far, _)| *far == 0).count();
        let width = (self.lines.iter())
            .filter(|(_, far, _)| *far <= around)
            .map(|(n, ..)| n.to_string().len())
            .max()
            .unwrap_or(1);
        let mut body = String::new();
        let mut at = 0;
        let mut skipped = 0;
        for (n, far, text) in &self.lines {
            if *far > around {
                skipped += 1;
                continue;
            }
            if skipped > 0 {
                body.push_str(&cut_lines(skipped));
                body.push('\n');
                skipped = 0;
            }
            if *far == 0 {
                at += 1;
                if let Some(c) = cut.filter(|&c| marked > 2 * c) {
                    if at == c + 1 {
                        body.push_str(&cut_lines(marked - 2 * c));
                        body.push('\n');
                    }
                    if at > c && at <= marked - c {
                        continue;
                    }
                }
            }
            let mark = if *far == 0 { '>' } else { ' ' };
            let _ = writeln!(body, "{mark} {n:>width$}  {text}");
        }
        if skipped > 0 {
            body.push_str(&cut_lines(skipped));
            body.push('\n');
        }
        body.pop();
        block(out, "", &body);
    }
}

/// A hunk of the file's diff, ready to write.
struct Piece {
    /// How far it is from the lines the prompt is about: 0 on them.
    far: usize,
    /// `@@ -a,b +c,d @@`, 1-based as git writes it.
    header: String,
    /// Each line's index on the side of those lines, or of the line after
    /// for one not on that side; its sign; its text.
    lines: Vec<(usize, char, String)>,
}

/// The file's hunks, each by how far it is from `anchor`'s lines; without
/// one, by its place in the file, the first nearest.
fn all_hunks(diff: &FileDiff, anchor: Option<&(Side, Range<usize>)>) -> Vec<Piece> {
    let side = anchor.map_or(Side::New, |(side, _)| *side);
    (diff.hunks.iter().enumerate())
        .map(|(i, h)| Piece {
            far: anchor.map_or(i + 1, |(side, lines)| distance(h, *side, lines)),
            header: header(h),
            lines: (h.lines.iter())
                .map(|l| {
                    let (sign, text) = match l.kind {
                        LineKind::Context => (' ', line(&diff.new, l.new)),
                        LineKind::Removed => ('-', line(&diff.old, l.old)),
                        LineKind::Added => ('+', line(&diff.new, l.new)),
                    };
                    let at = match side {
                        Side::Old => l.old,
                        Side::New => l.new,
                    };
                    (at, sign, text)
                })
                .collect(),
        })
        .collect()
}

/// Lines of `side` between `h` and `lines`, 0 when they meet.
fn distance(h: &Hunk, side: Side, lines: &Range<usize>) -> usize {
    let (start, len) = match side {
        Side::Old => (h.old_start, h.old_len),
        Side::New => (h.new_start, h.new_len),
    };
    let end = start + len.max(1);
    if start < lines.end && lines.start < end {
        0
    } else if end <= lines.start {
        lines.start - end + 1
    } else {
        start + 1 - lines.end
    }
}

fn header(h: &Hunk) -> String {
    let start = |at: usize, len: usize| if len == 0 { at } else { at + 1 };
    format!(
        "@@ -{},{} +{},{} @@",
        start(h.old_start, h.old_len),
        h.old_len,
        start(h.new_start, h.new_len),
        h.new_len
    )
}

/// A fenced block of `text`, with `info` after its opening fence.
fn block(out: &mut String, info: &str, text: &str) {
    let ticks = fence(text, 3);
    let _ = write!(out, "{ticks}{info}\n{text}\n{ticks}\n\n");
}

/// A prompt's parts, before trimming.
struct Parts<'a> {
    /// What it's about and where: never cut.
    head: String,
    /// The thread, its first comment first; none for a selection.
    comments: &'a [&'a Comment],
    /// The lines, and the words before them.
    code: Option<(&'static str, Code)>,
    /// The lines it's about, on their side, when the diff has them.
    anchor: Option<(Side, Range<usize>)>,
    /// The file's hunks, in file order.
    hunks: Vec<Piece>,
    /// The ask: never cut.
    tail: String,
}

/// How far a prompt is cut, each step on top of those before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Trim {
    /// The hunks not on the lines left out, the farthest first.
    dropped: usize,
    /// The hunks on the lines keep only those within `around` of them.
    window: bool,
    around: usize,
    /// The replies between the first comment and the last two left out,
    /// the earliest first, but those with a suggestion.
    omitted: usize,
    /// The commented lines kept only as their first and last this many.
    cut: Option<usize>,
}

impl Parts<'_> {
    fn fit(&self, cap: usize) -> Prompt {
        let mut t = Trim {
            dropped: 0,
            window: false,
            around: AROUND,
            omitted: 0,
            cut: None,
        };
        let fits = |t: &Trim| self.render(t).len() <= cap;
        let full = t;
        if !fits(&t) {
            let others = self.hunks.iter().filter(|h| h.far > 0).count();
            t.dropped = least(others, |n| fits(&Trim { dropped: n, ..t }));
        }
        if !fits(&t) {
            t.window = true;
        }
        while !fits(&t) && t.around > LEAST_AROUND {
            t.around -= 1;
        }
        if !fits(&t) {
            t.omitted = least(self.omittable().len(), |n| fits(&Trim { omitted: n, ..t }));
        }
        let marked = (self.code.iter())
            .flat_map(|(_, c)| &c.lines)
            .filter(|(_, far, _)| *far == 0)
            .count();
        if !fits(&t) && marked > 2 {
            // Each end keeps at most all but one of the middle lines.
            let most = (marked - 1) / 2;
            let fewer = least(most - 1, |n| {
                fits(&Trim {
                    cut: Some(most - n),
                    ..t
                })
            });
            t.cut = Some(most - fewer);
        }
        Prompt {
            text: self.render(&t),
            trimmed: t != full,
        }
    }

    /// The replies that can be left out: between the first comment and
    /// the last two, without a suggestion. The last two are the latest
    /// exchange, as an agent's answer and the follow-up to it.
    fn omittable(&self) -> Vec<usize> {
        (1..self.comments.len().saturating_sub(2))
            .filter(|&i| self.comments[i].suggestion.is_none())
            .collect()
    }

    fn render(&self, t: &Trim) -> String {
        let mut out = self.head.clone();
        let omitted = &self.omittable()[..t.omitted];
        let mut skipped = 0;
        for (i, comment) in self.comments.iter().enumerate() {
            if omitted.contains(&i) {
                skipped += 1;
                continue;
            }
            if skipped > 0 {
                let _ = if skipped == 1 {
                    write!(out, "[1 reply omitted]\n\n")
                } else {
                    write!(out, "[{skipped} replies omitted]\n\n")
                };
                skipped = 0;
            }
            let _ = writeln!(out, "{}:", who(&comment.author));
            out.push_str(comment.text.trim_end());
            out.push_str("\n\n");
            if let Some(suggestion) = &comment.suggestion {
                out.push_str("Suggested change, in place of the commented lines:\n\n");
                block(&mut out, "suggestion", suggestion);
            }
        }
        if let Some((intro, code)) = &self.code {
            out.push_str(intro);
            out.push_str("\n\n");
            code.render(t.around, t.cut, &mut out);
        }
        self.render_hunks(t, &mut out);
        out.push_str(&self.tail);
        out
    }

    fn render_hunks(&self, t: &Trim, out: &mut String) {
        if self.hunks.is_empty() {
            return;
        }
        // Nearest first; of two as near, the earlier.
        let mut others: Vec<usize> = (0..self.hunks.len())
            .filter(|&i| self.hunks[i].far > 0)
            .collect();
        others.sort_by_key(|&i| (self.hunks[i].far, i));
        let dropped = &others[others.len() - t.dropped..];
        let mut body = String::new();
        for (i, piece) in self.hunks.iter().enumerate() {
            if dropped.contains(&i) {
                continue;
            }
            body.push_str(&piece.header);
            body.push('\n');
            let window = (self.anchor.as_ref()).filter(|_| t.window && piece.far == 0);
            let mut skipped = 0;
            for (at, sign, text) in &piece.lines {
                let kept = window.is_none_or(|(side, lines)| {
                    let near =
                        lines.start.saturating_sub(t.around) <= *at && *at < lines.end + t.around;
                    // The other side's lines all sit at one place on this
                    // side, so a cut takes those among the commented ones
                    // whole.
                    let other = *sign == if *side == Side::New { '-' } else { '+' };
                    let middle = t.cut.is_some_and(|c| {
                        let (from, to) = if other {
                            (lines.start, lines.end)
                        } else {
                            (lines.start + c, lines.end - c)
                        };
                        lines.len() > 2 * c && from <= *at && *at < to
                    });
                    near && !middle
                });
                if !kept {
                    skipped += 1;
                    continue;
                }
                if skipped > 0 {
                    body.push_str(&cut_lines(skipped));
                    body.push('\n');
                    skipped = 0;
                }
                body.push(*sign);
                body.push_str(text);
                body.push('\n');
            }
            if skipped > 0 {
                body.push_str(&cut_lines(skipped));
                body.push('\n');
            }
        }
        let gone = if dropped.len() == self.hunks.len() {
            format!(
                "[the file's {} of changes omitted]",
                count(dropped.len(), "hunk")
            )
        } else {
            let intro = if self.anchor.is_some() {
                "The file's changes near them:"
            } else {
                "The file's changes:"
            };
            let _ = write!(out, "{intro}\n\n");
            body.pop();
            block(out, "diff", &body);
            if dropped.is_empty() {
                return;
            }
            format!(
                "[{} of this file omitted]",
                count(dropped.len(), "other hunk")
            )
        };
        out.push_str(&gone);
        out.push_str("\n\n");
    }
}

/// The least of `0..=most` that `fits`, or `most` when none does; `fits`
/// holds from some point on.
fn least(most: usize, fits: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, most);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if fits(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

#[cfg(test)]
mod tests;

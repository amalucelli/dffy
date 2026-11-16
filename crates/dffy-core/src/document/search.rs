//! Searching a file's diff for literal text, and the words a double click
//! selects.

use std::ops::Range;

use super::{Side, gaps};
use crate::diff::LineKind;
use crate::git::FileDiff;
use crate::view::display_text;

/// What a search looks for: a literal string, case ignored unless it has a
/// capital letter (smart case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needle {
    /// The string as typed.
    pub text: String,
    fold: bool,
}

impl Needle {
    /// A search for `text`.
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            fold: !text.chars().any(char::is_uppercase),
        }
    }

    /// Where it occurs in `line`, not overlapping, as byte ranges.
    #[must_use]
    pub fn find_all(&self, line: &[u8]) -> Vec<Range<usize>> {
        let needle = self.text.as_bytes();
        let mut out = Vec::new();
        if needle.is_empty() || needle.len() > line.len() {
            return out;
        }
        let mut i = 0;
        while i + needle.len() <= line.len() {
            let window = &line[i..i + needle.len()];
            let hit = if self.fold {
                window.eq_ignore_ascii_case(needle)
            } else {
                window == needle
            };
            if hit {
                out.push(i..i + needle.len());
                i += needle.len();
            } else {
                i += 1;
            }
        }
        out
    }
}

/// A match of a search: the side and line (0-based) it's on, and its
/// bytes within the line. Kept small: a one-letter search of a large file
/// has a match on most lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// The side whose text matched.
    pub side: Side,
    /// The line of that side.
    pub line: u32,
    /// First byte of the match within the line's text.
    pub start: u32,
    /// Past its last byte.
    pub end: u32,
}

/// A file's matches in the order the diff shows them (unified), and for
/// each side the matches on it by line, to find a row's matches while
/// drawing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Matches {
    /// Every match, in order.
    pub list: Vec<Match>,
    by_side: [Vec<u32>; 2],
}

impl Matches {
    /// The matches on `line` of `side`, as indices into `list`.
    pub fn on(&self, side: Side, line: usize) -> impl Iterator<Item = usize> + '_ {
        let ids = &self.by_side[side.index()];
        let line_of = |i: &u32| self.list[*i as usize].line as usize;
        let from = ids.partition_point(|i| line_of(i) < line);
        ids[from..]
            .iter()
            .take_while(move |i| line_of(i) == line)
            .map(|&i| i as usize)
    }
}

/// Every match of `needle` on the lines a file's diff shows: removed
/// lines' old text, added and unchanged lines' new text (an unchanged line
/// counts once), in the order the diff shows them. `hidden` has the new
/// lines each gap of unchanged lines hides under its separator, by gap (see
/// [`super::Document::hidden_lines`]); without it, only the hunks' lines count.
/// `None` once `cancelled` says so: it's asked every 4096 lines, so a
/// search that's been typed past stops.
pub fn find(
    diff: &FileDiff,
    hidden: Option<&[Range<usize>]>,
    needle: &Needle,
    cancelled: impl Fn() -> bool,
) -> Option<Matches> {
    let mut matches = Matches::default();
    let mut lines = 0usize;
    let mut scan = |side: Side, line: usize| {
        if lines.is_multiple_of(4096) && cancelled() {
            return false;
        }
        lines += 1;
        let text = match side {
            Side::Old => diff.old.line(line),
            Side::New => diff.new.line(line),
        };
        let text = display_text(text.strip_suffix(b"\n").unwrap_or(text));
        let line = u32::try_from(line).unwrap_or(u32::MAX);
        for bytes in needle.find_all(text) {
            let id = u32::try_from(matches.list.len()).unwrap_or(u32::MAX);
            matches.by_side[side.index()].push(id);
            matches.list.push(Match {
                side,
                line,
                start: u32::try_from(bytes.start).unwrap_or(u32::MAX),
                end: u32::try_from(bytes.end).unwrap_or(u32::MAX),
            });
        }
        true
    };
    for (g, gap) in gaps(diff).iter().enumerate() {
        if let Some(hidden) = hidden {
            let skip = hidden.get(g).cloned().unwrap_or(gap.new..gap.new + gap.len);
            for line in (gap.new..skip.start).chain(skip.end..gap.new + gap.len) {
                if !scan(Side::New, line) {
                    return None;
                }
            }
        }
        let Some(hunk) = diff.hunks.get(g) else {
            continue;
        };
        for l in &hunk.lines {
            let more = match l.kind {
                LineKind::Removed => scan(Side::Old, l.old),
                LineKind::Added | LineKind::Context => scan(Side::New, l.new),
            };
            if !more {
                return None;
            }
        }
    }
    Some(matches)
}

/// What a double click groups characters by.
#[derive(PartialEq)]
enum Class {
    Word,
    Blank,
    Other,
}

/// The word around byte `at` of `text`, as byte offsets: letters, digits
/// and `_`, or a run of blanks, or the one other character there. A line
/// that isn't valid UTF-8 gives the byte at `at`.
#[must_use]
pub fn word(text: &[u8], at: usize) -> (usize, usize) {
    let s = String::from_utf8_lossy(text);
    // Lossy text has the same offsets when the line is valid UTF-8.
    if s.len() != text.len() {
        return (at, (at + 1).min(text.len()));
    }
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            Class::Word
        } else if c == ' ' || c == '\t' {
            Class::Blank
        } else {
            Class::Other
        }
    };
    let Some(here) = s.get(at..).and_then(|rest| rest.chars().next()) else {
        return (at, at);
    };
    let kind = class(here);
    if kind == Class::Other {
        return (at, at + here.len_utf8());
    }
    let start = s[..at]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| class(c) == kind)
        .last()
        .map_or(at, |(i, _)| i);
    let end = s[at..]
        .char_indices()
        .find(|&(_, c)| class(c) != kind)
        .map_or(s.len(), |(i, _)| at + i);
    (start, end)
}

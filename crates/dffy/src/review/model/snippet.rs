//! What a thread whose lines weren't found shows of them: the code it was
//! written on, parsed in its file's language as its comments are placed
//! off the main thread ([`super::land`]), never while drawing.

#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

use dffy_core::annotation::{Anchor, Annotation, Placement};
use dffy_core::diff::Text;
use dffy_core::document::Side;
use dffy_core::highlight;
use dffy_core::view::{Run, Spans, Syntax};

#[cfg(test)]
use super::annotation;
use super::{CodeRow, Snippet};
use crate::review::{self, Comment, LineNumbers};

/// The syntax runs of each lost annotation's anchored code (see [`super::land`]).
pub(super) fn paint_lost(
    annotations: &[Annotation],
    placements: &[Placement],
) -> HashMap<String, Spans> {
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

/// What a thread whose lines weren't found shows of them, from its first
/// comment; `None` for a comment on the whole file. `painted`, the syntax
/// runs of its anchor's lines with their context (see [`super::land`]),
/// colors them.
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
    let lang = crate::grammars().detect(path, &bytes[..bytes.len().min(highlight::SNIFF_BYTES)])?;
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

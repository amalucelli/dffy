//! Token-level emphasis inside changed lines. Removed and added lines of a
//! change block are paired in order, the pair's tokens are diffed, and the
//! changed tokens become emphasis ranges. Pairs that share too little are left
//! unpaired so unrelated rewrites don't light up as noise.

use std::ops::Range;

use rustc_hash::FxHashMap;

use super::{Hunk, LineKind, MAX_INTRALINE_LINE_BYTES, Text, myers};
use crate::highlight::Spans;

/// How far ahead in the added lines a removed line looks for a partner. Lets
/// pairing recover from an inserted or deleted line without going quadratic
/// on large rewrites.
const PAIR_WINDOW: usize = 4;

/// Splits a line into identifiers/words, numbers, whitespace runs, and single
/// punctuation bytes. Bytes of a multi-byte UTF-8 sequence count as word
/// bytes. Ranges cover the whole input in order.
#[must_use]
pub fn tokenize(line: &[u8]) -> Vec<Range<usize>> {
    #[derive(PartialEq)]
    enum Class {
        Word,
        Space,
        Punct,
    }
    let class = |b: u8| {
        if b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80 {
            Class::Word
        } else if super::is_space(b) {
            Class::Space
        } else {
            Class::Punct
        }
    };
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < line.len() {
        let start = i;
        let c = class(line[i]);
        i += 1;
        if c == Class::Word && line[start].is_ascii_digit() {
            // Numbers keep their suffixes, radix letters and decimal points:
            // `0x1f`, `1.5e3`, `10UL`.
            while i < line.len()
                && (line[i].is_ascii_alphanumeric() || line[i] == b'_' || line[i] == b'.')
            {
                i += 1;
            }
        } else if c != Class::Punct {
            while i < line.len() && class(line[i]) == c {
                i += 1;
            }
        }
        tokens.push(start..i);
    }
    tokens
}

/// Intra-line emphasis for each line of `hunk`, in order. Lines without a
/// similar partner get none. [`super::hunks`] stores the same ranges in each
/// line; [`crate::git::Repo::file_diff`] leaves them out, so a viewer pays only
/// for the hunks it shows.
#[must_use]
pub fn emphasize(old: &Text, new: &Text, hunk: &Hunk) -> Vec<Emphasis> {
    emphasize_with(old, new, hunk, None, None)
}

/// [`emphasize`] with token boundaries from syntax spans: lines covered by
/// `old_spans` or `new_spans` split at leaf nodes ([`Spans::tokens`]), the
/// rest with [`tokenize`].
#[must_use]
pub fn emphasize_with(
    old: &Text,
    new: &Text,
    hunk: &Hunk,
    old_spans: Option<&Spans>,
    new_spans: Option<&Spans>,
) -> Vec<Emphasis> {
    paired_emphasis(old, new, hunk, old_spans, new_spans)
        .into_iter()
        .map(|(emphasis, _)| emphasis)
        .collect()
}

/// Sets [`super::Line::paired`] on the lines of `hunk` that [`emphasize`]
/// pairs, and clears it on the rest. Returns what [`emphasize`] would, for
/// a caller that needs both.
pub fn pair(old: &Text, new: &Text, hunk: &mut Hunk) -> Vec<Emphasis> {
    let paired = paired_emphasis(old, new, hunk, None, None);
    hunk.lines
        .iter_mut()
        .zip(paired)
        .map(|(line, (emphasis, paired))| {
            line.paired = paired;
            emphasis
        })
        .collect()
}

/// [`emphasize_with`], with whether each line was paired.
pub(crate) fn paired_emphasis(
    old: &Text,
    new: &Text,
    hunk: &Hunk,
    old_spans: Option<&Spans>,
    new_spans: Option<&Spans>,
) -> Vec<(Emphasis, bool)> {
    fn tokens<'a>(text: &'a Text, spans: Option<&Spans>, i: usize) -> Tokens<'a> {
        match spans {
            Some(spans) => Tokens::new(content(text.line(i)), || spans.tokens(text, i)),
            None => Tokens::plain(text.line(i)),
        }
    }
    let lines = &hunk.lines;
    let mut out = vec![(Vec::new(), false); lines.len()];
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind == LineKind::Context {
            i += 1;
            continue;
        }
        // A change block: removed lines, then added lines, up to context or
        // a line that starts the next block.
        let start = i;
        let continues = |i: usize| i == start || !lines[i].block_start;
        while i < lines.len() && lines[i].kind == LineKind::Removed && continues(i) {
            i += 1;
        }
        let plus = i;
        while i < lines.len() && lines[i].kind == LineKind::Added && continues(i) {
            i += 1;
        }
        pair_block(
            plus - start,
            i - plus,
            |k| tokens(old, old_spans, lines[start + k].old),
            |k| tokens(new, new_spans, lines[plus + k].new),
            |a, b, old_emph, new_emph| {
                out[start + a] = (old_emph, true);
                out[plus + b] = (new_emph, true);
            },
        );
    }
    out
}

/// Emphasis for a change block given as its removed and added lines, for
/// callers that hold lines rather than a [`Text`] (a parsed patch). Lines may
/// keep their terminators. Calls `set(i, j, removed_emphasis, added_emphasis)`
/// for each paired removed line `i` and added line `j`; unpaired lines get no
/// emphasis. Pairing is the same as for [`super::hunks`].
pub fn emphasize_lines(
    removed: &[&[u8]],
    added: &[&[u8]],
    set: impl FnMut(usize, usize, Emphasis, Emphasis),
) {
    pair_block(
        removed.len(),
        added.len(),
        |i| Tokens::plain(removed[i]),
        |j| Tokens::plain(added[j]),
        set,
    );
}

/// Whether `old` and `new` share enough to pair as one edited into the
/// other, by the test that pairs lines for emphasis. Either may span
/// several lines; text over [`MAX_INTRALINE_LINE_BYTES`] never pairs.
#[must_use]
pub fn similar(old: &[u8], new: &[u8]) -> bool {
    emphasize_pair(
        &Tokens::plain(old),
        &Tokens::plain(new),
        &mut FxHashMap::default(),
    )
    .is_some()
}

struct Tokens<'a> {
    content: &'a [u8],
    tokens: Vec<Range<usize>>,
}

impl<'a> Tokens<'a> {
    /// Lines too long to pair aren't tokenized at all.
    fn new(content: &'a [u8], tokens: impl FnOnce() -> Vec<Range<usize>>) -> Self {
        Tokens {
            content,
            tokens: if content.len() > MAX_INTRALINE_LINE_BYTES {
                Vec::new()
            } else {
                tokens()
            },
        }
    }

    fn plain(line: &'a [u8]) -> Self {
        let content = content(line);
        Tokens::new(content, || tokenize(content))
    }
}

fn pair_block<'a>(
    removed: usize,
    added: usize,
    old_line: impl Fn(usize) -> Tokens<'a>,
    new_line: impl Fn(usize) -> Tokens<'a>,
    mut set: impl FnMut(usize, usize, Emphasis, Emphasis),
) {
    let mut next_plus = 0;
    // An added line is tried against up to PAIR_WINDOW removed lines, so
    // its tokens are kept once made.
    let mut news: Vec<Option<Tokens>> = std::iter::repeat_with(|| None).take(added).collect();
    let mut ids = FxHashMap::default();
    for i in 0..removed {
        let old = old_line(i);
        for j in (next_plus..added).take(PAIR_WINDOW) {
            let new = news[j].get_or_insert_with(|| new_line(j));
            if let Some((old_emph, new_emph)) = emphasize_pair(&old, new, &mut ids) {
                set(i, j, old_emph, new_emph);
                next_plus = j + 1;
                break;
            }
        }
    }
}

/// A line without its terminator.
fn content(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Changed byte ranges within one line.
pub type Emphasis = Vec<Range<usize>>;

/// Diffs the tokens of two lines. Returns the changed byte ranges of each, or
/// `None` if the lines are too long or too different to pair. `ids` is
/// scratch space for interning, kept between calls for its allocation.
fn emphasize_pair<'a>(
    old: &Tokens<'a>,
    new: &Tokens<'a>,
    ids: &mut FxHashMap<&'a [u8], u32>,
) -> Option<(Emphasis, Emphasis)> {
    let (old_tokens, new_tokens) = (&old.tokens, &new.tokens);
    let (old, new) = (old.content, new.content);
    if old.len() > MAX_INTRALINE_LINE_BYTES || new.len() > MAX_INTRALINE_LINE_BYTES {
        return None;
    }
    ids.clear();
    let mut intern = |line: &'a [u8], tokens: &[Range<usize>]| -> Vec<u32> {
        tokens
            .iter()
            .map(|r| {
                let next = u32::try_from(ids.len()).unwrap_or(u32::MAX);
                *ids.entry(&line[r.clone()]).or_insert(next)
            })
            .collect()
    };
    let a = intern(old, old_tokens);
    let b = intern(new, new_tokens);
    let mut removed = vec![false; a.len()];
    let mut added = vec![false; b.len()];
    myers::diff(&a, &b, &mut removed, &mut added);

    let solid = |line: &[u8], r: &Range<usize>| {
        line[r.clone()]
            .iter()
            .filter(|&&b| !super::is_space(b))
            .count()
    };
    let common: usize = old_tokens
        .iter()
        .zip(&removed)
        .filter(|&(_, &changed)| !changed)
        .map(|(r, _)| solid(old, r))
        .sum();
    let total = old_tokens.iter().map(|r| solid(old, r)).sum::<usize>()
        + new_tokens.iter().map(|r| solid(new, r)).sum::<usize>();
    // Pair when at least 40% of the non-whitespace bytes are shared (delta's
    // default `max-line-distance` of 0.6).
    if common * 5 < total {
        return None;
    }
    Some((ranges(old_tokens, &removed), ranges(new_tokens, &added)))
}

/// Merges the changed tokens into maximal contiguous byte ranges.
fn ranges(tokens: &[Range<usize>], changed: &[bool]) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    for (r, _) in tokens.iter().zip(changed).filter(|&(_, &c)| c) {
        match out.last_mut() {
            Some(last) if last.end == r.start => last.end = r.end,
            _ => out.push(r.clone()),
        }
    }
    out
}

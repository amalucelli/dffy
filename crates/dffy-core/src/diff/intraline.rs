//! Token-level emphasis inside changed lines. Removed and added lines of a
//! change block are paired in order, the pair's tokens are diffed, and the
//! changed tokens become emphasis ranges. Pairs that share too little are left
//! unpaired so unrelated rewrites don't light up as noise.

use std::ops::Range;

use rustc_hash::FxHashMap;

use super::{Hunk, LineKind, MAX_INTRALINE_LINE_BYTES, Text, myers};
use crate::view::{Kind, Spans};

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
    pair_blocks(
        lines.len(),
        |i| Some(lines[i].kind),
        |i| lines[i].block_start,
        |i| tokens(old, old_spans, lines[i].old),
        |i| tokens(new, new_spans, lines[i].new),
    )
}

/// [`pair`] for a hunk held as its lines rather than as two [`Text`]s, such
/// as one read from a patch: `line(i)` gives each of the `len` lines' kind
/// and its text without the sign (a terminator may stay). Returns each
/// line's emphasis and whether it was paired. A no-newline marker among a
/// change block's lines is skipped, as if it weren't there.
pub fn pair_lines<'a>(
    len: usize,
    line: impl Fn(usize) -> (Kind, &'a [u8]),
) -> Vec<(Emphasis, bool)> {
    pair_blocks(
        len,
        |i| match line(i).0 {
            Kind::Context => Some(LineKind::Context),
            Kind::Removed => Some(LineKind::Removed),
            Kind::Added => Some(LineKind::Added),
            Kind::NoNewline => None,
        },
        |_| false,
        |i| Tokens::plain(line(i).1),
        |i| Tokens::plain(line(i).1),
    )
}

/// Emphasis and pairing for `len` hunk lines, block by block. A change
/// block is removed lines, then added lines, up to context or a line that
/// `block_start`s the next block. `kind` is `None` for a line of neither
/// side (a no-newline marker), which a block takes in and pairing skips.
fn pair_blocks<'a>(
    len: usize,
    kind: impl Fn(usize) -> Option<LineKind>,
    block_start: impl Fn(usize) -> bool,
    old_line: impl Fn(usize) -> Tokens<'a>,
    new_line: impl Fn(usize) -> Tokens<'a>,
) -> Vec<(Emphasis, bool)> {
    let mut out = vec![(Vec::new(), false); len];
    let mut i = 0;
    while i < len {
        if kind(i) == Some(LineKind::Context) {
            i += 1;
            continue;
        }
        let start = i;
        let continues = |i: usize| i == start || !block_start(i);
        while i < len && matches!(kind(i), Some(LineKind::Removed) | None) && continues(i) {
            i += 1;
        }
        let plus = i;
        while i < len && matches!(kind(i), Some(LineKind::Added) | None) && continues(i) {
            i += 1;
        }
        if plus == start || i == plus {
            continue;
        }
        pair_block(
            start..plus,
            plus..i,
            |k| kind(k).is_some(),
            &old_line,
            &new_line,
            |a, b, old_emph, new_emph| {
                out[a] = (old_emph, true);
                out[b] = (new_emph, true);
            },
        );
    }
    out
}

/// Emphasis for a change block given as its removed and added lines, for
/// callers that hold lines rather than a [`Text`]. Lines may keep their
/// terminators. Calls `set(i, j, removed_emphasis, added_emphasis)` for each
/// paired removed line `i` and added line `j`; unpaired lines get no
/// emphasis. Pairing is the same as for [`super::hunks`].
pub fn emphasize_lines(
    removed: &[&[u8]],
    added: &[&[u8]],
    mut set: impl FnMut(usize, usize, Emphasis, Emphasis),
) {
    // One index space for both: removed lines first, then added ones.
    let r = removed.len();
    pair_block(
        0..r,
        r..r + added.len(),
        |_| true,
        |i| Tokens::plain(removed[i]),
        |j| Tokens::plain(added[j - r]),
        |i, j, old_emph, new_emph| set(i, j - r, old_emph, new_emph),
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

/// Pairs the removed lines `removed` with the added lines `added`, both
/// hunk line indices; indices that aren't `is_line` (markers) are skipped.
/// Calls `set(i, j, removed_emphasis, added_emphasis)` for each pair.
fn pair_block<'a>(
    removed: Range<usize>,
    added: Range<usize>,
    is_line: impl Fn(usize) -> bool,
    old_line: impl Fn(usize) -> Tokens<'a>,
    new_line: impl Fn(usize) -> Tokens<'a>,
    mut set: impl FnMut(usize, usize, Emphasis, Emphasis),
) {
    let mut next_plus = added.start;
    // An added line is tried against up to PAIR_WINDOW removed lines, so
    // its tokens are kept once made.
    let mut news: Vec<Option<Tokens>> = std::iter::repeat_with(|| None).take(added.len()).collect();
    let mut ids = FxHashMap::default();
    for i in removed.filter(|&i| is_line(i)) {
        let old = old_line(i);
        for j in (next_plus..added.end)
            .filter(|&j| is_line(j))
            .take(PAIR_WINDOW)
        {
            let new = news[j - added.start].get_or_insert_with(|| new_line(j));
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

//! A file's rendered view: the blocks a renderer plugin drew of each side,
//! laid out as lines of text and diffed block by block. The result is a
//! core [`File`] over that text, which the core keeps each line's styles
//! and source lines for, so tints, word emphasis, search, `n`/`N` and the
//! gaps work on it as on any diff. What's here is the plugin protocol's
//! side: laying the blocks out, pairing edited blocks, and the emphasis of
//! restyled words.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::ops::Range;
use std::sync::Arc;

use dffy_core::diff::{
    Emphasis, Hunk, Line, LineKind, MAX_INTRALINE_LINE_BYTES, Text, Unchanged, emphasize_lines,
    similar, tokenize, unit_hunks,
};
use dffy_core::document::{File, Origin, Rendered, Side};
use dffy_core::git::FileDiff;
use dffy_core::highlight::{self, Language, Registry};
use dffy_core::view::{Attributes, Paint, Syntax};

use super::Copied;
use crate::plugin;

/// How many added blocks ahead a removed block looks for the one it was
/// edited into; bounds the similarity tests in a long rewrite.
const PAIR_WINDOW: usize = 8;

/// A copy of rendered lines `lo..=hi` of `side` of `rendered`: the source
/// of every block they touch, whole.
pub(super) fn copy(rendered: &Rendered, side: Side, lo: usize, hi: usize) -> Copied {
    let lines = rendered.source_lines(side, lo, hi);
    let source = match side {
        Side::Old => &rendered.source().old,
        Side::New => &rendered.source().new,
    };
    let mut text: Vec<u8> = lines
        .clone()
        .flat_map(|i| source.line(i))
        .copied()
        .collect();
    // No line break after the last line: pasted into a shell, it would
    // run what was copied.
    if text.ends_with(b"\n") {
        text.pop();
    }
    Copied {
        text,
        lines: lines.len(),
        chars: None,
        decorated: false,
        hidden: 0,
        missing: 0,
    }
}

/// The language a code block's fence names: `rust`, or an extension such
/// as `rs`.
fn language<'g>(grammars: &'g Registry, name: &str) -> Option<&'g Language> {
    let name = name.trim().to_ascii_lowercase();
    grammars
        .named(&name)
        .or_else(|| grammars.detect(format!("x.{name}").as_bytes(), b""))
}

/// Syntax colors for a code block's lines, from the bundled highlighter;
/// `None` when its language is unknown or it doesn't parse in time.
fn highlighted(grammars: &Registry, block: &plugin::Block) -> Option<Vec<Vec<Paint>>> {
    let lang = language(grammars, block.language.as_deref()?)?;
    let mut bytes = Vec::new();
    for line in &block.lines {
        bytes.extend_from_slice(line.text.as_bytes());
        bytes.push(b'\n');
    }
    let text = Text::new(bytes);
    let spans = highlight::parse(lang, text.bytes())?.spans(&text, 0..text.len());
    Some(
        (0..text.len())
            .map(|i| {
                spans
                    .line(i)
                    .iter()
                    .map(|run| Paint {
                        start: run.start,
                        end: run.end,
                        syntax: Some(run.syntax),
                        attributes: Attributes::NONE,
                    })
                    .collect()
            })
            .collect(),
    )
}

/// A block's identity: its kind, its words line by line with the style
/// and link target of each, and its lines of `source` less trailing
/// whitespace. Blocks are unchanged only when their identities match, so
/// a change in a word's style, a link's target or anything else in the
/// source shows as a change, as the line diff would show it; only the
/// layout of the rendered text (table padding, where lines wrap) may
/// differ.
fn identity(block: &plugin::Block, source: &Text) -> u64 {
    let mut hasher = DefaultHasher::new();
    block.kind.hash(&mut hasher);
    for line in &block.lines {
        let styles = styles(line);
        let bytes = line.text.as_bytes();
        for word in tokenize(bytes) {
            if bytes[word.clone()].iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            bytes[word.clone()].hash(&mut hasher);
            for i in word {
                styles.at(i).hash(&mut hasher);
            }
        }
        hasher.write_u8(0xff);
    }
    for i in block.source.clone().filter(|&i| i < source.len()) {
        source.line(i).trim_ascii_end().hash(&mut hasher);
    }
    hasher.finish()
}

/// A rendered line's styles by byte, as the renderer gave them: its paint
/// and its links' targets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Styles {
    paint: Vec<Paint>,
    links: Vec<(Range<usize>, u64)>,
}

impl Styles {
    /// The style of byte `i`: its paint's syntax color and attributes, and
    /// the hash of its link's target.
    fn at(&self, i: usize) -> (Option<Syntax>, Attributes, u64) {
        let paint = self
            .paint
            .iter()
            .find(|p| (p.start as usize..p.end as usize).contains(&i));
        let link = self.links.iter().find(|(r, _)| r.contains(&i));
        (
            paint.and_then(|p| p.syntax),
            paint.map_or(Attributes::NONE, |p| p.attributes),
            link.map_or(0, |l| l.1),
        )
    }
}

fn styles(line: &plugin::Line) -> Styles {
    Styles {
        paint: line.paint.clone(),
        links: line
            .links
            .iter()
            .map(|(r, url)| {
                let mut hasher = DefaultHasher::new();
                url.hash(&mut hasher);
                (r.clone(), hasher.finish() | 1)
            })
            .collect(),
    }
}

/// One side laid out: its text, its blocks as diff units with their kinds,
/// and for each line its block, the block's identity, origin and styles.
#[derive(Default)]
struct Layout {
    bytes: Vec<u8>,
    units: Vec<(u64, usize)>,
    kinds: Vec<String>,
    /// Each block's words alone, whitespace and styles aside: blocks whose
    /// words match are one edited into the other, never the same block.
    words: Vec<u64>,
    blocks: Vec<u32>,
    identities: Vec<u64>,
    origins: Vec<Origin>,
    /// The renderer's styles, for telling a restyled word.
    styles: Vec<Styles>,
    paint: Vec<Vec<Paint>>,
}

impl Layout {
    /// The lines of each block from `lines`' first to its last, in order,
    /// as `(block, lines)`.
    fn blocks_in(&self, lines: Range<usize>) -> Vec<(usize, Range<usize>)> {
        let mut out: Vec<(usize, Range<usize>)> = Vec::new();
        for line in lines {
            let block = self.blocks.get(line).map_or(usize::MAX, |&b| b as usize);
            match out.last_mut() {
                Some((b, range)) if *b == block => range.end = line + 1,
                _ => out.push((block, line..line + 1)),
            }
        }
        out
    }
}

/// One side of the rendered view, from the blocks drawn of `source`, with
/// code blocks highlighted in `grammars`' languages.
fn lay_out(side: plugin::Side, source: &Text, grammars: &Registry) -> Layout {
    let mut out = Layout::default();
    for block in side.blocks.into_iter().filter(|b| !b.lines.is_empty()) {
        let id = identity(&block, source);
        let mut words = DefaultHasher::new();
        for line in &block.lines {
            for word in line.text.split_whitespace() {
                word.hash(&mut words);
            }
            words.write_u8(0xff);
        }
        out.words.push(words.finish());
        let colors = highlighted(grammars, &block);
        let index = u32::try_from(out.units.len()).unwrap_or(u32::MAX);
        out.units.push((id, block.lines.len()));
        let (start, end) = (
            u32::try_from(block.source.start).unwrap_or(u32::MAX),
            u32::try_from(block.source.end).unwrap_or(u32::MAX),
        );
        for (i, line) in block.lines.into_iter().enumerate() {
            out.bytes.extend_from_slice(line.text.as_bytes());
            out.bytes.push(b'\n');
            out.blocks.push(index);
            out.identities.push(id);
            out.origins.push(Origin {
                start,
                end,
                first: i == 0,
            });
            out.styles.push(styles(&line));
            let paint = match &colors {
                Some(colors) => colors.get(i).cloned().unwrap_or_default(),
                None => line.paint,
            };
            out.paint.push(paint);
        }
        out.kinds.push(block.kind);
    }
    out
}

/// Lays out each run of changed lines of `hunk` so a removed block and the
/// added block it was edited into, of the same kind and with similar text,
/// form a change block of their own, away from the rest of the run: line
/// pairing, and so word emphasis and split view's rows, then keep to the
/// pair. Pairs are found within the run, in order. Without this, pairing
/// lines across the whole run lets a line of one block take another's
/// partner (two blank lines always pair), and the edited block shows as
/// removed and added.
fn pair_blocks(hunk: &mut Hunk, sides: [&Layout; 2], texts: [&Text; 2]) {
    let lines = std::mem::take(&mut hunk.lines);
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind == LineKind::Context {
            out.push(lines[i].clone());
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && lines[i].kind == LineKind::Removed {
            i += 1;
        }
        let plus = i;
        while i < lines.len() && lines[i].kind == LineKind::Added {
            i += 1;
        }
        let (first, last) = (&lines[start], &lines[i - 1]);
        let old = match lines[start..plus] {
            [] => first.old..first.old,
            [ref a, ..] => a.old..lines[plus - 1].old + 1,
        };
        let new = match lines[plus..i] {
            [] => last.new..last.new,
            [ref a, ..] => a.new..last.new + 1,
        };
        rearrange(old, new, sides, texts, &mut out);
    }
    hunk.lines = out;
}

/// Word emphasis for each line of `hunk`, a hunk of `diff`, its lines'
/// renderer styles `styles`: the line diff's, pairing lines the same way,
/// plus every word on a paired line whose text is the same on both sides
/// but whose style or link target isn't.
fn emphasis(diff: &FileDiff, hunk: &Hunk, styles: [&[Styles]; 2]) -> Vec<Emphasis> {
    fn content(text: &Text, i: usize) -> &[u8] {
        let line = text.line(i);
        line.strip_suffix(b"\n").unwrap_or(line)
    }
    let lines = &hunk.lines;
    let mut out = vec![Vec::new(); lines.len()];
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind == LineKind::Context {
            i += 1;
            continue;
        }
        let start = i;
        let continues = |i: usize| i == start || !lines[i].block_start;
        while i < lines.len() && lines[i].kind == LineKind::Removed && continues(i) {
            i += 1;
        }
        let plus = i;
        while i < lines.len() && lines[i].kind == LineKind::Added && continues(i) {
            i += 1;
        }
        let removed: Vec<&[u8]> = lines[start..plus]
            .iter()
            .map(|l| content(&diff.old, l.old))
            .collect();
        let added: Vec<&[u8]> = lines[plus..i]
            .iter()
            .map(|l| content(&diff.new, l.new))
            .collect();
        emphasize_lines(&removed, &added, |r, a, mut old_emph, mut new_emph| {
            let (o, n) = (lines[start + r].old, lines[plus + a].new);
            let unchanged = |text: &[u8], emph: &[Range<usize>]| {
                tokenize(text)
                    .into_iter()
                    .filter(|t| !emph.iter().any(|e| e.contains(&t.start)))
                    .collect::<Vec<_>>()
            };
            let (old_words, new_words) = (
                unchanged(removed[r], &old_emph),
                unchanged(added[a], &new_emph),
            );
            let style = |side: usize, line: usize, i: usize| {
                styles[side].get(line).map(|s| s.at(i)).unwrap_or_default()
            };
            for (ow, nw) in old_words.into_iter().zip(new_words) {
                let blank = removed[r][ow.clone()].iter().all(u8::is_ascii_whitespace);
                if !blank
                    && ow
                        .clone()
                        .zip(nw.clone())
                        .any(|(x, y)| style(0, o, x) != style(1, n, y))
                {
                    old_emph.push(ow);
                    new_emph.push(nw);
                }
            }
            out[start + r] = merged(old_emph);
            out[plus + a] = merged(new_emph);
        });
    }
    out
}

/// `ranges` sorted, with touching ones joined.
fn merged(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if last.end >= r.start => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// Whether two blocks' rendered text `old` and `new` are alike enough to be
/// one edited into the other: by the test that pairs edited lines, or for
/// text too long for it, by the share of lines they have in common, at the
/// same 40%, so a long code block with a line edited still pairs.
fn alike(old: &[u8], new: &[u8]) -> bool {
    if old.len().max(new.len()) <= MAX_INTRALINE_LINE_BYTES {
        return similar(old, new);
    }
    let mut lines: HashMap<&[u8], usize> = HashMap::new();
    for line in old.split(|&b| b == b'\n') {
        *lines.entry(line.trim_ascii()).or_default() += 1;
    }
    let mut common = 0;
    for line in new.split(|&b| b == b'\n') {
        if let Some(n) = lines.get_mut(line.trim_ascii()).filter(|n| **n > 0) {
            *n -= 1;
            common += 1;
        }
    }
    let total = old.split(|&b| b == b'\n').count() + new.split(|&b| b == b'\n').count();
    common * 5 >= total
}

/// Appends the changed lines `old` and `new` of one run, laid out as
/// [`pair_blocks`] says.
fn rearrange(
    old: Range<usize>,
    new: Range<usize>,
    sides: [&Layout; 2],
    texts: [&Text; 2],
    out: &mut Vec<Line>,
) {
    let removed = sides[0].blocks_in(old.clone());
    let added = sides[1].blocks_in(new.clone());
    let text = |side: usize, lines: &Range<usize>| {
        let t = texts[side];
        &t.bytes()[t.offset(lines.start)..t.offset(lines.end)]
    };
    let kind = |side: usize, block: usize| sides[side].kinds.get(block).map(String::as_str);
    let words = |side: usize, block: usize| sides[side].words.get(block).copied();
    let mut pairs = Vec::new();
    let mut next = 0;
    for (r, (rb, rl)) in removed.iter().enumerate() {
        let found = (next..added.len()).take(PAIR_WINDOW).find(|&a| {
            let (ab, al) = &added[a];
            kind(0, *rb) == kind(1, *ab)
                && (words(0, *rb) == words(1, *ab) || alike(text(0, rl), text(1, al)))
        });
        if let Some(a) = found {
            pairs.push((r, a));
            next = a + 1;
        }
    }
    let mut at = (old.start, new.start);
    let mut push = |kind: LineKind, lines: Range<usize>, block_start: bool| {
        for (k, line) in lines.enumerate() {
            let (o, n) = match kind {
                LineKind::Removed => (line, at.1),
                _ => (at.0, line),
            };
            out.push(Line {
                kind,
                old: o,
                new: n,
                emphasis: Vec::new(),
                unchanged: Unchanged::No,
                block_start: block_start && k == 0,
                paired: false,
            });
            match kind {
                LineKind::Removed => at.0 = line + 1,
                _ => at.1 = line + 1,
            }
        }
    };
    let (mut r, mut a, mut first) = (0, 0, true);
    let span = |blocks: &[(usize, Range<usize>)], from: usize, to: usize| {
        blocks[from].1.start..blocks[to - 1].1.end
    };
    let end = (removed.len(), added.len());
    for &(pr, pa) in pairs.iter().chain(std::iter::once(&end)) {
        if r < pr {
            push(LineKind::Removed, span(&removed, r, pr), !first);
            first = false;
        }
        if a < pa {
            push(LineKind::Added, span(&added, a, pa), !first);
            first = false;
        }
        if pr < removed.len() {
            push(LineKind::Removed, removed[pr].1.clone(), !first);
            push(LineKind::Added, added[pa].1.clone(), false);
            first = false;
        }
        (r, a) = (pr + 1, pa + 1);
    }
}

/// The rendered view of `source`, the line diff of the file `sides` were
/// drawn from at `width`, with `context` lines around each change and code
/// blocks highlighted in `grammars`' languages. Edited blocks pair as
/// [`pair_blocks`] says, and a changed block whose identity is also among
/// the other side's changed blocks is marked moved.
pub(crate) fn build(
    source: Arc<FileDiff>,
    sides: [plugin::Side; 2],
    grammars: &Registry,
    context: usize,
    width: usize,
) -> File {
    let [old, new] = sides;
    let (mut old, mut new) = (
        lay_out(old, &source.old, grammars),
        lay_out(new, &source.new, grammars),
    );
    let (old_text, new_text) = (
        Text::new(std::mem::take(&mut old.bytes)),
        Text::new(std::mem::take(&mut new.bytes)),
    );
    let mut hunks = unit_hunks(&old_text, &new_text, [&old.units, &new.units], context);
    for hunk in &mut hunks {
        pair_blocks(hunk, [&old, &new], [&old_text, &new_text]);
    }
    let changed = |kind: LineKind, ids: &[u64]| -> HashSet<u64> {
        hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == kind)
            .filter_map(|l| {
                ids.get(if kind == LineKind::Removed {
                    l.old
                } else {
                    l.new
                })
            })
            .copied()
            .collect()
    };
    let (removed, added) = (
        changed(LineKind::Removed, &old.identities),
        changed(LineKind::Added, &new.identities),
    );
    let spacer = |side: &Layout, i: usize| side.origins.get(i).is_some_and(|o| o.spacer());
    for line in hunks.iter_mut().flat_map(|h| &mut h.lines) {
        let moved = match line.kind {
            LineKind::Removed => {
                !spacer(&old, line.old)
                    && old
                        .identities
                        .get(line.old)
                        .is_some_and(|id| added.contains(id))
            }
            LineKind::Added => {
                !spacer(&new, line.new)
                    && new
                        .identities
                        .get(line.new)
                        .is_some_and(|id| removed.contains(id))
            }
            LineKind::Context => false,
        };
        if moved {
            line.unchanged = Unchanged::Moved;
        }
    }
    let diff = FileDiff {
        change: source.change.clone(),
        old: old_text,
        new: new_text,
        binary: false,
        too_large: false,
        note: None,
        hunks,
    };
    let mut file = File::rendered_view(
        source,
        diff,
        [old.origins, new.origins],
        [old.paint, new.paint],
        width,
        "renders the same; r shows the lines",
    );
    let diff = Arc::clone(file.diff());
    for (h, hunk) in diff.hunks.iter().enumerate() {
        let emphasis = emphasis(&diff, hunk, [&old.styles, &new.styles]);
        file.set_emphasis(&diff, h, emphasis, None);
    }
    file
}

#[cfg(test)]
mod tests {
    use dffy_core::diff::{Options, Whitespace, hunks};
    use dffy_core::document::{self, Pos, Render, Source, State};
    use dffy_core::git::{FileChange, Mode, Status};
    use dffy_core::view::{self, Kind};

    use super::super::{Doc, Selection};
    use super::*;
    use crate::config::Indicators;
    use crate::render::Layout;

    const OLD: &str =
        "# Title\n\nMoved para.\n\nThe quick brown fox.\n\nGone paragraph here.\n\nKeep.\n";
    const NEW: &str =
        "# Title\n\nThe quick red fox.\n\nFresh words entirely.\n\nKeep.\n\nMoved para.\n";

    fn block(kind: &str, source: Range<usize>, text: &str) -> plugin::Block {
        plugin::Block {
            kind: kind.to_owned(),
            source,
            lines: vec![plugin::Line {
                text: text.to_owned(),
                paint: Vec::new(),
                links: Vec::new(),
            }],
            language: None,
        }
    }

    /// A document showing a README's line diff.
    fn doc() -> (Doc, Arc<FileDiff>) {
        doc_of(OLD, NEW)
    }

    /// A document showing the line diff of a README from `old` to `new`.
    fn doc_of(old: &str, new: &str) -> (Doc, Arc<FileDiff>) {
        let side = Some(dffy_core::git::Side::Worktree {
            mode: Mode::File,
            id: None,
        });
        let (old, new) = (Text::new(old.into()), Text::new(new.into()));
        let fd = FileDiff {
            change: FileChange {
                path: "README.md".into(),
                status: Status::Modified,
                old: side,
                new: side,
                staging: None,
            },
            hunks: hunks(&old, &new, &Options::default()),
            old,
            new,
            binary: false,
            too_large: false,
            note: None,
        };
        let layout = Layout {
            split: false,
            width: Some(60),
            wrap: false,
            words: false,
            tab_width: 4,
            indicators: Indicators::Classic,
            one_sided: false,
            digits: 0,
            inline_signs: false,
            scroll: 0,
            line_numbers: true,
            hunk_headers: true,
            modified_lines: true,
        };
        let mut doc = Doc::new(layout, 20);
        doc.document.push(fd.change.clone(), Source::Repo);
        let file = File::prepare(crate::grammars(), None).file(fd, Whitespace::Exact, false);
        assert!(doc.load(0, Ok(Box::new(file))));
        let diff = Arc::clone(doc.document.diff_of(0).expect("loaded"));
        (doc, diff)
    }

    fn sides() -> [plugin::Side; 2] {
        let old = vec![
            block("heading", 0..1, "Title"),
            block("paragraph", 2..3, "Moved para."),
            block("paragraph", 4..5, "The quick brown fox."),
            block("paragraph", 6..7, "Gone paragraph here."),
            block("paragraph", 8..9, "Keep."),
        ];
        let new = vec![
            block("heading", 0..1, "Title"),
            block("paragraph", 2..3, "The quick red fox."),
            block("paragraph", 4..5, "Fresh words entirely."),
            block("paragraph", 6..7, "Keep."),
            block("paragraph", 8..9, "Moved para."),
        ];
        [plugin::Side { blocks: old }, plugin::Side { blocks: new }]
    }

    /// Each line of the view's hunks: its sign (`»` for moved, `~` for a
    /// line paired with its edit), its text, and its gutter numbers.
    fn lines(loaded: &File) -> Vec<String> {
        assert!(loaded.rendered().is_some(), "a rendered view");
        // Rendered text ends every line, so a hunk's body has no
        // no-newline markers: its lines are the core hunk's, in order.
        loaded
            .diff()
            .hunks
            .iter()
            .enumerate()
            .flat_map(|(h, hunk)| {
                hunk.lines
                    .iter()
                    .enumerate()
                    .map(move |(i, l)| (loaded.lines(false, h).line(i), l))
            })
            .map(|(line, l)| {
                let sign = match (l.kind, l.unchanged, l.paired) {
                    (_, Unchanged::Moved, _) => '»',
                    (LineKind::Context, ..) => ' ',
                    (_, _, true) => '~',
                    (LineKind::Removed, ..) => '-',
                    (LineKind::Added, ..) => '+',
                };
                let n = |n: Option<usize>| n.map_or_else(|| ".".to_owned(), |n| n.to_string());
                format!(
                    "{sign} {} {} {}",
                    n(line.old),
                    n(line.new),
                    String::from_utf8_lossy(line.text)
                )
            })
            .collect()
    }

    /// The emphasis of line `i` of the view's first hunk.
    fn emphasized(loaded: &File, i: usize) -> &[Range<usize>] {
        loaded.lines(false, 0).line(i).emphasis
    }

    /// Blocks change whole: the edited paragraph pairs with its old self
    /// for word emphasis, a block that moved is marked moved, and every
    /// line is numbered with its block's source line.
    #[test]
    fn blocks_diff_whole_and_number_their_source() {
        let (_, diff) = doc();
        let loaded = build(Arc::clone(&diff), sides(), crate::grammars(), 3, 60);
        assert_eq!(
            lines(&loaded),
            [
                "  1 1 Title",
                "» 3 . Moved para.",
                "~ 5 . The quick brown fox.",
                "~ . 3 The quick red fox.",
                "- 7 . Gone paragraph here.",
                "+ . 5 Fresh words entirely.",
                "  9 7 Keep.",
                "» . 9 Moved para.",
            ]
        );
    }

    /// A paragraph as the Markdown plugin draws one: its text, then the
    /// blank line the source has after it.
    fn para(source: usize, text: &str) -> plugin::Block {
        let mut block = block("paragraph", source..source + 1, text);
        block.lines.push(plugin::Line {
            text: String::new(),
            paint: Vec::new(),
            links: Vec::new(),
        });
        block
    }

    /// An edited paragraph between unchanged ones, next to a removed one,
    /// pairs with its old self: both its lines show modified, side by side
    /// in split view, with the changed word emphasized. The blank lines
    /// ending each paragraph don't pair with each other in its place.
    #[test]
    fn an_edited_paragraph_between_unchanged_ones_is_a_modified_pair() {
        let old = "Intro.\n\nGone para here.\n\nBeta three four.\n\nTail.\n";
        let new = "Intro.\n\nBeta three FOUR.\n\nTail.\n";
        let (_, diff) = doc_of(old, new);
        let sides = [
            plugin::Side {
                blocks: vec![
                    para(0, "Intro."),
                    para(2, "Gone para here."),
                    para(4, "Beta three four."),
                    para(6, "Tail."),
                ],
            },
            plugin::Side {
                blocks: vec![
                    para(0, "Intro."),
                    para(2, "Beta three FOUR."),
                    para(4, "Tail."),
                ],
            },
        ];
        let loaded = build(diff, sides, crate::grammars(), 3, 60);
        assert_eq!(
            lines(&loaded),
            [
                "  1 1 Intro.",
                "  . . ",
                "- 3 . Gone para here.",
                "- . . ",
                "~ 5 . Beta three four.",
                "~ . . ",
                "~ . 3 Beta three FOUR.",
                "~ . . ",
                "  7 5 Tail.",
                "  . . ",
            ]
        );
        let hunk = &loaded.diff().hunks[0];
        for line in [4, 6] {
            let emphasis = emphasized(&loaded, line);
            assert_eq!(emphasis.first(), Some(&(11..15)), "line {line}");
            assert_eq!(emphasis.len(), 1, "line {line}");
        }
        let layout = Layout {
            split: true,
            ..doc_of(old, new).0.layout
        };
        let header = document::hunk_header(hunk, b"");
        let mut rows = Vec::new();
        view::rows(
            layout.arrangement(&header),
            hunk.lines.len(),
            |i| {
                let kind = match hunk.lines[i].kind {
                    LineKind::Context => Kind::Context,
                    LineKind::Removed => Kind::Removed,
                    LineKind::Added => Kind::Added,
                };
                (kind, hunk.lines[i].block_start)
            },
            &mut rows,
        );
        assert!(
            rows.contains(&view::Row::Pair(Some(4), Some(6))),
            "{rows:?}"
        );
        assert!(rows.contains(&view::Row::Pair(Some(2), None)), "{rows:?}");
    }

    /// `block` with `style` (and `url`) on the bytes of its first line
    /// where `word` is.
    fn styled(
        mut block: plugin::Block,
        word: &str,
        style: Paint,
        url: Option<&str>,
    ) -> plugin::Block {
        let line = &mut block.lines[0];
        let start = line.text.find(word).expect("the word");
        let end = start + word.len();
        line.paint.push(Paint {
            start: u32::try_from(start).expect("short"),
            end: u32::try_from(end).expect("short"),
            ..style
        });
        if let Some(url) = url {
            line.links.push((start..end, url.to_owned()));
        }
        block
    }

    /// A word that becomes a code span, a link whose target changes but not
    /// its text, and a word made bold each show as a modified pair, the
    /// restyled words emphasized on both sides, though the words are the
    /// same.
    #[test]
    fn restyled_words_and_changed_link_targets_are_changes() {
        let old = "Install dffy first.\n\nSee the [docs](a.md).\n\n- make it bold\n- other\n";
        let new = "Install `dffy` first.\n\nSee the [docs](b.md).\n\n- make it **bold**\n- other\n";
        let (_, diff) = doc_of(old, new);
        let code = Paint {
            start: 0,
            end: 0,
            syntax: Some(Syntax::String),
            attributes: Attributes::NONE,
        };
        let link = Paint {
            syntax: Some(Syntax::Function),
            attributes: Attributes::UNDERLINE,
            ..code
        };
        let bold = Paint {
            syntax: None,
            attributes: Attributes::BOLD,
            ..code
        };
        let side = |code_span: bool, url: &str, bolded: bool| {
            let install = para(0, "Install dffy first.");
            let item = block("list-item", 4..5, "• make it bold");
            plugin::Side {
                blocks: vec![
                    if code_span {
                        styled(install, "dffy", code, None)
                    } else {
                        install
                    },
                    styled(para(2, "See the docs."), "docs", link, Some(url)),
                    if bolded {
                        styled(item, "bold", bold, None)
                    } else {
                        item
                    },
                    block("list-item", 5..6, "• other"),
                ],
            }
        };
        let loaded = build(
            diff,
            [side(false, "a.md", false), side(true, "b.md", true)],
            crate::grammars(),
            3,
            60,
        );
        assert_eq!(
            lines(&loaded),
            [
                "~ 1 . Install dffy first.",
                "~ . . ",
                "~ . 1 Install dffy first.",
                "~ . . ",
                "~ 3 . See the docs.",
                "~ . . ",
                "~ . 3 See the docs.",
                "~ . . ",
                "~ 5 . • make it bold",
                "~ . 5 • make it bold",
                "  6 6 • other",
            ]
        );
        let words = |i: usize| -> Vec<&str> {
            let diff = loaded.diff();
            let l = &diff.hunks[0].lines[i];
            let text = if l.kind == LineKind::Removed {
                diff.old.line(l.old)
            } else {
                diff.new.line(l.new)
            };
            emphasized(&loaded, i)
                .iter()
                .map(|r| std::str::from_utf8(&text[r.clone()]).expect("utf-8"))
                .collect()
        };
        for (i, word) in [
            (0, "dffy"),
            (2, "dffy"),
            (4, "docs"),
            (6, "docs"),
            (8, "bold"),
            (9, "bold"),
        ] {
            assert_eq!(words(i), [word], "line {i}");
        }
    }

    /// A code block too long for the line-pairing test pairs with its edit
    /// by the lines they share, the edited line emphasized.
    #[test]
    fn a_long_code_block_with_a_line_edited_is_a_modified_pair() {
        let code = |edited: bool| {
            let lines: Vec<String> = (0..300)
                .map(|i| match i {
                    150 if edited => "let value = 151;".to_owned(),
                    i => format!("let value = {i};"),
                })
                .collect();
            let mut block = block("code", 0..302, &lines[0]);
            block.lines = lines
                .iter()
                .map(|text| plugin::Line {
                    text: text.clone(),
                    paint: Vec::new(),
                    links: Vec::new(),
                })
                .collect();
            block
        };
        let source = |edited: bool| {
            let mut out = "```\n".to_owned();
            for line in &code(edited).lines {
                out.push_str(&line.text);
                out.push('\n');
            }
            out + "```\n"
        };
        let (_, diff) = doc_of(&source(false), &source(true));
        let sides = [false, true].map(|edited| plugin::Side {
            blocks: vec![code(edited)],
        });
        let loaded = build(diff, sides, crate::grammars(), 3, 60);
        let lines = &loaded.diff().hunks[0].lines;
        assert!(lines.iter().all(|l| l.paired), "every line pairs");
        let changed: Vec<usize> = (0..lines.len())
            .filter(|&i| !emphasized(&loaded, i).is_empty())
            .collect();
        assert_eq!(changed, [150, 450]);
    }

    /// A reference link's target changes in a line no block holds; the
    /// paragraph using it shows as a modified pair, its link emphasized.
    #[test]
    fn a_reference_links_new_target_is_a_change() {
        let (old, new) = (
            "See [docs][d].\n\n[d]: a.md\n",
            "See [docs][d].\n\n[d]: b.md\n",
        );
        let Some(sides) = markdown(old, new) else {
            eprintln!("skipped: dffy-render-markdown isn't built");
            return;
        };
        let (_, diff) = doc_of(old, new);
        let loaded = build(diff, sides, crate::grammars(), 3, 60);
        assert_eq!(
            lines(&loaded),
            ["~ 1 . See docs.", "~ . 1 See docs.", "  . . "]
        );
        assert_eq!(emphasized(&loaded, 0).first(), Some(&(4..8)));
        assert_eq!(emphasized(&loaded, 1).first(), Some(&(4..8)));
    }

    /// A blank line drawn from no source line is never moved: removing a
    /// paragraph and its gap here and adding another there leaves one gap
    /// removed and one added, as blank lines in a line diff.
    #[test]
    fn a_gap_removed_here_and_added_there_is_not_moved() {
        let (old, new) = (
            "Alpha.\n\nGone para.\n\nKeep.\n",
            "Alpha.\n\nKeep.\n\nFresh text.\n",
        );
        let side = |paragraphs: [&str; 3]| plugin::Side {
            blocks: paragraphs
                .iter()
                .enumerate()
                .flat_map(|(i, text)| {
                    let line = 2 * i;
                    [
                        block("paragraph", line..line + 1, text),
                        block("gap", line + 1..line + 1, ""),
                    ]
                })
                .collect(),
        };
        let (_, diff) = doc_of(old, new);
        let sides = [
            side(["Alpha.", "Gone para.", "Keep."]),
            side(["Alpha.", "Keep.", "Fresh text."]),
        ];
        let shown = lines(&build(diff, sides, crate::grammars(), 3, 60));
        assert!(
            shown.iter().any(|l| l == "- . . ") && shown.iter().any(|l| l == "+ . . "),
            "{shown:?}"
        );
        assert!(!shown.iter().any(|l| l.starts_with('»')), "{shown:?}");
    }

    /// Both sides of a Markdown file as the Markdown plugin built with the
    /// workspace draws them; `None` when it isn't built.
    fn markdown(old: &str, new: &str) -> Option<[plugin::Side; 2]> {
        let exe = std::env::current_exe().ok()?;
        let program = exe.parent()?.parent()?.join("dffy-render-markdown");
        if !program.exists() {
            return None;
        }
        let request = plugin::Request {
            path: "README.md",
            old: Some(old),
            new: Some(new),
            width: 60,
            light: false,
        };
        let mut child = std::process::Command::new(program)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("the plugin starts");
        std::io::Write::write_all(
            &mut child.stdin.take().expect("stdin"),
            request.encode().as_bytes(),
        )
        .expect("the request");
        let answer = child.wait_with_output().expect("the answer");
        let lines = [Text::new(old.into()).len(), Text::new(new.into()).len()];
        Some(plugin::decode(&answer.stdout, lines.map(Some)).expect("a valid answer"))
    }

    /// A small Markdown document: headings, paragraphs with links, and a
    /// list, from `seed`'s words.
    struct Document {
        pieces: Vec<Vec<String>>,
        items: Vec<Vec<String>>,
        level: usize,
    }

    impl Document {
        fn text(&self) -> String {
            let mut out = format!("{} Title\n\n", "#".repeat(self.level));
            for words in &self.pieces {
                out.push_str(&words.join(" "));
                out.push_str("\n\n");
            }
            for item in &self.items {
                out.push_str("- ");
                out.push_str(&item.join(" "));
                out.push('\n');
            }
            out.push_str("\nThe end.\n");
            out
        }
    }

    /// Every source line the line diff counts as changed lies in a block
    /// the rendered view shows as changed, whatever the edit: a word made
    /// code, bold or italic, a word replaced, a link's target changed, a
    /// list item moved, a heading's level changed.
    #[test]
    fn every_changed_source_line_is_in_a_changed_block() {
        if markdown("a\n", "a\n").is_none() {
            eprintln!(
                "skipped: dffy-render-markdown isn't built; cargo build --workspace builds it"
            );
            return;
        }
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % n as u64).expect("small")
        };
        for case in 0..120 {
            let mut words = |n: usize| -> Vec<String> {
                (0..n)
                    .map(|_| WORDS[next(WORDS.len())].to_owned())
                    .collect()
            };
            let mut doc = Document {
                pieces: (0..4).map(|_| words(4)).collect(),
                items: (0..4).map(|_| words(3)).collect(),
                level: 1,
            };
            doc.pieces[1][1] = "[link](a.md)".to_owned();
            let old = doc.text();
            let mut moved = false;
            for _ in 0..=next(3) {
                moved |= doc.edit(&mut next);
            }
            let new = doc.text();
            check_rendering(&old, &new, moved, &format!("case {case}"));
        }
    }

    const WORDS: [&str; 8] = [
        "dffy", "diff", "fast", "the", "viewer", "terminal", "git", "tree",
    ];

    impl Document {
        /// One random edit; returns whether it moved a list item.
        fn edit(&mut self, next: &mut impl FnMut(usize) -> usize) -> bool {
            let (p, w) = (next(self.pieces.len()), next(4));
            let word = &mut self.pieces[p][w];
            match next(7) {
                0 => *word = format!("`{word}`"),
                1 => *word = format!("**{word}**"),
                2 => *word = format!("_{word}_"),
                3 => *word = WORDS[next(WORDS.len())].to_owned(),
                4 => self.pieces[1][1] = format!("[link](b{}.md)", next(9)),
                5 => {
                    let item = self.items.remove(next(self.items.len()));
                    let at = next(self.items.len() + 1);
                    self.items.insert(at, item);
                    return true;
                }
                _ => self.level = 1 + next(3),
            }
            false
        }
    }

    /// Checks the rendered view of a README edited from `old` to `new`
    /// against its line diff; `moved` when a list item moved.
    fn check_rendering(old: &str, new: &str, moved: bool, case: &str) {
        let sides = markdown(old, new).expect("built");
        let (_, diff) = doc_of(old, new);
        let blocks: [Vec<(Range<usize>, Range<usize>)>; 2] = [0, 1].map(|s| {
            let mut at = 0;
            sides[s]
                .blocks
                .iter()
                .filter(|b| !b.lines.is_empty())
                .map(|b| {
                    at += b.lines.len();
                    (b.source.clone(), at - b.lines.len()..at)
                })
                .collect()
        });
        let loaded = build(Arc::clone(&diff), sides, crate::grammars(), 3, 60);
        let mut shown: [HashSet<usize>; 2] = Default::default();
        for l in loaded.diff().hunks.iter().flat_map(|h| &h.lines) {
            match l.kind {
                LineKind::Removed => shown[0].insert(l.old),
                LineKind::Added => shown[1].insert(l.new),
                LineKind::Context => false,
            };
        }
        let context = format!("{case}\nold:\n{old}\nnew:\n{new}");
        // What shows unchanged is unchanged: the blocks of the lines left
        // out of the changes, taken in order on both sides, have the same
        // source.
        let source = |side: usize, line: usize| -> Vec<Vec<u8>> {
            let text = if side == 0 { &diff.old } else { &diff.new };
            let (lines, _) = blocks[side]
                .iter()
                .find(|(_, rendered)| rendered.contains(&line))
                .expect("every rendered line is in a block");
            lines
                .clone()
                .map(|i| text.line(i).trim_ascii_end().to_vec())
                .collect()
        };
        let kept = |side: usize, total: usize| {
            (0..total)
                .filter(|i| !shown[side].contains(i))
                .collect::<Vec<_>>()
        };
        let kept = [
            kept(0, loaded.diff().old.len()),
            kept(1, loaded.diff().new.len()),
        ];
        assert_eq!(kept[0].len(), kept[1].len(), "{context}");
        for (&o, &n) in kept[0].iter().zip(&kept[1]) {
            assert_eq!(source(0, o), source(1, n), "{context}");
        }
        // And with nothing moved, which leaves the alignment of equal lines
        // free, every line the line diff counts as changed is in a block
        // shown changed.
        let lines = dffy_core::diff::changes(&diff.old, &diff.new, Whitespace::Exact);
        for (side, flags) in [(0, &lines.removed), (1, &lines.added)] {
            for line in (0..flags.len()).filter(|&i| flags[i] && !moved) {
                let holding: Vec<_> = blocks[side]
                    .iter()
                    .filter(|(source, _)| source.contains(&line))
                    .collect();
                assert!(
                    holding.is_empty()
                        || holding
                            .iter()
                            .any(|(_, r)| r.clone().any(|i| shown[side].contains(&i))),
                    "source line {line} of side {side} changed in a block shown unchanged; {context}"
                );
            }
        }
    }

    /// Hunk headers and the separators between hunks count the source
    /// lines the blocks were drawn from, blank lines between them
    /// included, as the gutter numbers them.
    #[test]
    fn headers_and_separators_count_source_lines() {
        let text = |edited: bool| {
            (0..8)
                .map(|k| match k {
                    5 if edited => "Para five.\n\n".to_owned(),
                    k => format!("Para {k}.\n\n"),
                })
                .collect::<String>()
        };
        let (mut doc, diff) = doc_of(&text(false), &text(true));
        let side = |edited: bool| plugin::Side {
            blocks: (0..8)
                .map(|k| {
                    let text = match k {
                        5 if edited => "Para five.".to_owned(),
                        k => format!("Para {k}."),
                    };
                    block("paragraph", 2 * k..2 * k + 1, &text)
                })
                .collect(),
        };
        let loaded = build(
            Arc::clone(&diff),
            [side(false), side(true)],
            crate::grammars(),
            1,
            60,
        );
        assert!(doc.set_rendered(0, &diff, Ok(Box::new(loaded))));
        doc.set_rendered_mode(true);
        doc.document.top();
        let mut grid = crate::term::Grid::new(60, 8);
        doc.height = 8;
        doc.draw(
            &mut grid,
            0,
            60,
            0,
            &crate::theme::Theme::default_theme(),
            1,
        );
        let rows: Vec<String> = (0..7)
            .map(|y| grid.row_text(y).trim_end().to_owned())
            .collect();
        assert_eq!(
            rows,
            [
                "⋯ 8 unchanged lines  @@ -9,6 +9,6 @@ Para 3.",
                " 9  9  │ Para 4.",
                "11    -│ Para 5.",
                "   11 +│ Para five.",
                "13 13  │ Para 6.",
                "⋯ 2 unchanged lines",
                "",
            ]
        );
    }

    /// A code block naming a language dffy knows is highlighted by dffy,
    /// in place of the renderer's own styles.
    #[test]
    fn code_blocks_take_the_bundled_highlighter() {
        let (_, diff) = doc();
        let mut code = block("code", 0..1, "fn main() {}");
        code.language = Some("rs".to_owned());
        code.lines[0].paint.push(Paint {
            start: 0,
            end: 12,
            syntax: Some(Syntax::String),
            attributes: Attributes::NONE,
        });
        let sides = [plugin::Side::default(), plugin::Side { blocks: vec![code] }];
        let loaded = build(diff, sides, crate::grammars(), 3, 60);
        let rendered = loaded.rendered().expect("a rendered view");
        let keyword = rendered.paint(Side::New, 0).first().copied();
        assert_eq!(
            keyword.map(|p| (p.start, p.end, p.syntax)),
            Some((0, 2, Some(Syntax::Keyword)))
        );
    }

    /// The view swaps with the line diff both ways without losing either,
    /// and a copy takes the selected blocks' source.
    #[test]
    fn the_rendered_view_toggles_and_copies_its_source() {
        let (mut doc, diff) = doc();
        assert!(doc.set_rendered(
            0,
            &diff,
            Ok(Box::new(build(
                Arc::clone(&diff),
                sides(),
                crate::grammars(),
                3,
                60
            )))
        ));
        assert!(matches!(
            doc.document.files()[0].render(),
            Some(Render::Hidden(_))
        ));
        doc.set_rendered_mode(true);
        let rendered = |doc: &Doc| matches!(doc.document.files()[0].state(), State::Loaded(l) if l.rendered().is_some());
        assert!(rendered(&doc));
        assert!(doc.document.files()[0].wants_render().is_none());
        doc.selection = Some(Selection {
            side: Side::New,
            anchor: Pos::default(),
            cursor: Pos::default(),
            lines: (1, 2),
            bytes: None,
        });
        let copied = doc.selected(false).expect("a copy");
        assert_eq!(copied.text, b"The quick red fox.\n\nFresh words entirely.");
        assert_eq!(copied.lines, 3);
        doc.set_rendered_mode(false);
        assert!(!rendered(&doc));
        assert!(Arc::ptr_eq(doc.document.diff_of(0).expect("loaded"), &diff));
        doc.set_rendered_mode(true);
        assert!(rendered(&doc));
    }
}

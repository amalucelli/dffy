//! A rendered view: a [`File`] whose diff is of text a renderer drew from a
//! file's two sides (a Markdown document laid out as paragraphs and
//! tables), block by block. The text alone can't say what each rendered
//! line looks like or which source lines it stands for; this keeps both, so
//! the view numbers, heads and counts its rows by the source and draws
//! each line in the renderer's styles.

use std::ops::Range;
use std::sync::Arc;

use super::{File, Side};
use crate::diff::{Hunk, LineKind, Whitespace};
use crate::git::FileDiff;
use crate::view::{HunkHeader, Kind, Line, Paint};

/// The source lines a rendered line's block was drawn from, and whether
/// it's the block's first line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Origin {
    /// First source line of the block, 0-based.
    pub start: u32,
    /// Past its last source line; equal to `start` for a line drawn from no
    /// source line, as the blank line a renderer leaves between blocks.
    pub end: u32,
    /// The block's first rendered line.
    pub first: bool,
}

impl Origin {
    /// Drawn from no source line: it has no line number, and like a blank
    /// line in a line diff it's never marked moved.
    #[inline]
    #[must_use]
    pub fn spacer(self) -> bool {
        self.start == self.end
    }
}

/// How a rendered view maps to the file it was drawn from.
pub struct Rendered {
    /// The line diff it was drawn from.
    source: Arc<FileDiff>,
    /// Per side, per rendered line.
    origins: [Vec<Origin>; 2],
    /// Per side, for each rendered line and the end, the source line that
    /// stretch of the view starts at: its block's first line, or past the
    /// block for a block's later lines. Never decreasing, from 0 to the
    /// side's last line, so any run of rendered lines maps to the source
    /// lines it stands for, blank lines between blocks included.
    bounds: [Vec<u32>; 2],
    paint: [Vec<Vec<Paint>>; 2],
    width: usize,
}

/// `line` as drawn: unchanged for a line diff, restyled by `rendered` for
/// a rendered view.
#[inline]
pub(super) fn restyle<'a>(rendered: Option<&'a Rendered>, line: Line<'a>) -> Line<'a> {
    match rendered {
        Some(rendered) => rendered.restyle(line),
        None => line,
    }
}

impl Rendered {
    /// `line`, a line of this view as the diff has it, as it's drawn: in
    /// its styles, and numbered with its block's first source line on the
    /// block's first line only. A moved line keeps its own look, as a
    /// moved line of code drops its syntax colors.
    #[inline]
    fn restyle<'a>(&'a self, mut line: Line<'a>) -> Line<'a> {
        let (side, number) = if line.state.kind() == Kind::Removed {
            (Side::Old, line.old)
        } else {
            (Side::New, line.new)
        };
        if !line.state.is_unchanged()
            && let Some(n) = number
        {
            line.paint = self.paint(side, n - 1);
        }
        line.old = line.old.and_then(|n| self.number(Side::Old, n - 1));
        line.new = line.new.and_then(|n| self.number(Side::New, n - 1));
        line
    }

    #[inline]
    fn number(&self, side: Side, line: usize) -> Option<usize> {
        let origin = self.origins[side.index()].get(line)?;
        (origin.first && !origin.spacer()).then_some(origin.start as usize + 1)
    }

    /// The renderer's styles for rendered line `line` (0-based) of `side`.
    #[inline]
    #[must_use]
    pub fn paint(&self, side: Side, line: usize) -> &[Paint] {
        self.paint[side.index()]
            .get(line)
            .map_or(&[], Vec::as_slice)
    }

    /// The line diff it was drawn from.
    #[inline]
    #[must_use]
    pub fn source(&self) -> &Arc<FileDiff> {
        &self.source
    }

    /// The width it was drawn for, as the host gave it.
    #[inline]
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// The source lines that rendered lines `lo..=hi` of `side` came from:
    /// every line of every block they touch.
    #[must_use]
    pub fn source_lines(&self, side: Side, lo: usize, hi: usize) -> Range<usize> {
        let origins = &self.origins[side.index()];
        let touched = origins.get(lo..=hi.min(origins.len().saturating_sub(1)));
        let start = touched.and_then(|o| o.iter().map(|o| o.start).min());
        let end = touched.and_then(|o| o.iter().map(|o| o.end).max());
        start.zip(end).map_or(0..0, |(s, e)| s as usize..e as usize)
    }

    /// Lines of the longer source side, which line numbers go up to.
    #[must_use]
    pub fn source_len(&self) -> usize {
        self.source.old.len().max(self.source.new.len())
    }

    /// The source lines rendered lines `lines` of `side` stand for.
    fn source_span(&self, side: Side, lines: Range<usize>) -> Range<usize> {
        let bounds = &self.bounds[side.index()];
        let at = |i: usize| {
            bounds
                .get(i)
                .or_else(|| bounds.last())
                .map_or(0, |&b| b as usize)
        };
        at(lines.start)..at(lines.end)
    }

    /// Source lines among rendered lines `lines` of the old side.
    pub(super) fn old_count(&self, lines: Range<usize>) -> usize {
        self.source_span(Side::Old, lines).len()
    }

    /// `hunk`'s numbers in source lines, as line numbers have them.
    pub(super) fn header<'a>(&self, hunk: &Hunk, heading: &'a [u8]) -> HunkHeader<'a> {
        let old = self.source_span(Side::Old, hunk.old_start..hunk.old_start + hunk.old_len);
        let new = self.source_span(Side::New, hunk.new_start..hunk.new_start + hunk.new_len);
        let one_based = |r: &Range<usize>| if r.is_empty() { r.start } else { r.start + 1 };
        HunkHeader {
            old_start: one_based(&old),
            old_len: old.len(),
            new_start: one_based(&new),
            new_len: new.len(),
            heading,
            hidden: 0,
        }
    }
}

/// [`Rendered::bounds`] for one side's `origins`, whose source has `lines`
/// lines.
fn bounds(origins: &[Origin], lines: usize) -> Vec<u32> {
    let end = u32::try_from(lines).unwrap_or(u32::MAX);
    let mut at = 0;
    let mut out = Vec::with_capacity(origins.len() + 1);
    for (i, o) in origins.iter().enumerate() {
        if i > 0 {
            at = at.max(if o.first { o.start } else { o.end }).min(end);
        }
        out.push(at);
    }
    out.push(end.max(at));
    out
}

impl File {
    /// A rendered view of `source`, a line diff: `diff` is of the text a
    /// renderer drew of each side, `origins` and `paint` give each rendered
    /// line's source lines and styles per side, and `width` is what it was
    /// drawn for. Its counts are `source`'s, and when the rendered sides
    /// are the same it notes `same`. Lines are paired; emphasis is the
    /// caller's to set ([`File::set_emphasis`]).
    #[must_use]
    pub fn rendered_view(
        source: Arc<FileDiff>,
        diff: FileDiff,
        origins: [Vec<Origin>; 2],
        paint: [Vec<Vec<Paint>>; 2],
        width: usize,
        same: &'static str,
    ) -> Self {
        let mut file = Self::new(diff, Whitespace::Exact, None);
        if file.diff.hunks.is_empty() {
            file.note = Some(same);
        }
        file.counts = super::Counts::default();
        for line in source.hunks.iter().flat_map(|h| &h.lines) {
            match line.kind {
                LineKind::Added => file.counts.added += 1,
                LineKind::Removed => file.counts.removed += 1,
                LineKind::Context => {}
            }
        }
        let paint_bytes: usize = paint
            .iter()
            .flat_map(|side| side.iter())
            .map(|p| 24 + p.len() * size_of::<Paint>())
            .sum();
        file.bytes += paint_bytes + (origins[0].len() + origins[1].len()) * size_of::<Origin>();
        let bounds = [
            bounds(&origins[0], source.old.len()),
            bounds(&origins[1], source.new.len()),
        ];
        file.bytes += (bounds[0].len() + bounds[1].len()) * size_of::<u32>();
        file.rendered = Some(Box::new(Rendered {
            source,
            origins,
            bounds,
            paint,
            width,
        }));
        file
    }
}

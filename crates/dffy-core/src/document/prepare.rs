//! The work that builds and completes a [`File`], split at the granularity
//! hosts draw at, so nothing is computed for parts never shown.

use std::ops::Range;
use std::sync::{Arc, OnceLock};

use gix::bstr::ByteSlice;

use super::{File, sources};
use crate::cache::Cache;
use crate::diff::{Emphasis, Hunk, Text, Whitespace, emphasize};
use crate::git::FileDiff;
use crate::highlight::{FileSyntax, Highlighter, HunkSyntax, Registry, Source};
use crate::structural::{self, Structural, Unavailable};
use crate::view::Spans;

/// A file's highlighting, prepared by whichever worker first needs it;
/// workers asking at the same time wait for that one.
pub type SyntaxSlot = Arc<OnceLock<FileSyntax>>;

/// The per-file work, with the grammars to highlight and syntax-diff in and
/// the disk cache for large sides' results, if any. Each step is a separate
/// call at the granularity a host draws: a viewer builds a [`File`] with
/// [`Prepare::file`] (no side parsed), then asks for
/// [`Prepare::emphasis`] for each hunk it draws, [`Prepare::gap_syntax`]
/// for each gap whose lines it draws, [`Prepare::scopes`] once the file's
/// highlighting is ready, and [`Prepare::structural`] for the files on
/// screen while the syntax diff is on. Print streams a file
/// without building a [`File`]: [`super::whitespace_only`], a
/// [`Prepare::highlighter`] per side on threads it schedules, or on one
/// thread the old side's then [`Prepare::highlighter_after`],
/// [`Prepare::structural`] when asked, then [`pair`].
#[derive(Clone, Copy)]
pub struct Prepare<'a> {
    grammars: &'a Registry,
    cache: Option<&'a Cache>,
}

impl File {
    /// The per-file pipeline, with `grammars` and `cache`. Each step is its
    /// own call, so a host runs exactly the ones its frame or output needs.
    #[must_use]
    pub fn prepare<'a>(grammars: &'a Registry, cache: Option<&'a Cache>) -> Prepare<'a> {
        Prepare { grammars, cache }
    }
}

impl Prepare<'_> {
    /// `diff` prepared for display, every hunk paired; with `highlight`, a
    /// [`SyntaxSlot`] for sides in a known language, filled by the first
    /// [`Prepare::emphasis`] or [`Prepare::gap_syntax`] call. No side is
    /// parsed here.
    #[must_use]
    pub fn file(&self, diff: FileDiff, whitespace: Whitespace, highlight: bool) -> File {
        File::new(diff, whitespace, highlight.then_some(self.grammars))
    }

    /// [`Prepare::file`] for a [`super::Source::Unchanged`] entry's diff, a
    /// file compared with itself: every line is in one gap, which the
    /// entry always shows.
    #[must_use]
    pub fn unchanged(&self, diff: FileDiff, highlight: bool) -> File {
        File::unchanged(diff, highlight.then_some(self.grammars))
    }

    /// Both sides of `diff` prepared for highlighting, on two threads when
    /// both are large (see [`FileSyntax::new`]). With `store`, large sides'
    /// spans go to the cache.
    #[must_use]
    pub fn syntax(&self, diff: &FileDiff, store: bool) -> FileSyntax {
        FileSyntax::new(self.grammars, sources(diff), &diff.hunks, self.cache, store)
    }

    /// One side prepared for highlighting, without storing its spans in the
    /// cache; `None` when it can't be highlighted (see
    /// [`Highlighter::for_side`]).
    #[must_use]
    pub fn highlighter(&self, side: Source) -> Option<Highlighter> {
        Highlighter::for_side(self.grammars, side, self.cache, false)
    }

    /// The new side `side` prepared for highlighting from the old side's
    /// highlighter, text and the line diff's hunks, without storing its
    /// spans in the cache (see [`Highlighter::after`]).
    #[must_use]
    pub fn highlighter_after(
        &self,
        side: Source,
        old: Option<(&Highlighter, &Text, &[Hunk])>,
    ) -> Option<Highlighter> {
        Highlighter::after(self.grammars, side, old, self.cache, false)
    }

    /// The emphasis of hunk `hunk` of `diff`, and its syntax spans when
    /// `slot` holds a highlighted file's. The first call for a file fills
    /// the slot, parsing both sides and storing large ones' spans in the
    /// cache; the parse gives the emphasis its tokens too.
    #[must_use]
    pub fn emphasis(
        &self,
        diff: &FileDiff,
        hunk: usize,
        slot: Option<&SyntaxSlot>,
    ) -> (Vec<Emphasis>, Option<HunkSyntax>) {
        let core = &diff.hunks[hunk];
        let syntax = slot
            .map(|slot| slot.get_or_init(|| self.syntax(diff, true)))
            .filter(|s| !s.is_empty())
            .map(|s| s.hunk(&diff.old, &diff.new, core));
        let emphasis = match &syntax {
            Some(spans) => spans.emphasis(&diff.old, &diff.new, core),
            None => emphasize(&diff.old, &diff.new, core),
        };
        (emphasis, syntax)
    }

    /// The syntax runs of the new-side `lines` of a gap of `diff`, filling
    /// `slot` as [`Prepare::emphasis`] does; `None` when the new side isn't
    /// highlighted.
    #[must_use]
    pub fn gap_syntax(
        &self,
        diff: &FileDiff,
        slot: &SyntaxSlot,
        lines: Range<usize>,
    ) -> Option<Spans> {
        slot.get_or_init(|| self.syntax(diff, true))
            .new_spans(&diff.new, lines)
    }

    /// Finds the definitions `slot`'s highlighting of `diff` left to find
    /// (see [`FileSyntax::find_scopes`]), for [`File::scopes`]. Does
    /// nothing before the slot is filled: it never parses.
    pub fn scopes(&self, diff: &FileDiff, slot: &SyntaxSlot) {
        if let Some(syntax) = slot.get() {
            syntax.find_scopes(&diff.old, &diff.new);
        }
    }

    /// `diff`'s syntax diff, through the cache for large files: `None` for
    /// files with nothing to diff (binary, too large, only a mode change).
    #[must_use]
    pub fn structural(&self, diff: &FileDiff) -> Option<Result<Structural, Unavailable>> {
        let ids = [
            super::cache_id(diff.change.old),
            super::cache_id(diff.change.new),
        ];
        (!diff.binary && !diff.too_large && !diff.hunks.is_empty()).then(|| {
            let path = diff.change.path.as_bytes();
            structural::diff_cached(
                self.grammars,
                path,
                &diff.old,
                &diff.new,
                &diff.hunks,
                ids,
                self.cache,
            )
        })
    }
}

/// Pairs the lines of each of `hunks`, of `old` and `new` (see
/// [`crate::diff::pair`]). With `keep`, returns each hunk's emphasis, found
/// on the way, for a caller without syntax tokens to replace it; without,
/// returns nothing and allocates nothing for it.
pub fn pair(old: &Text, new: &Text, hunks: &mut [Hunk], keep: bool) -> Vec<Vec<Emphasis>> {
    let mut kept = Vec::new();
    for hunk in hunks {
        let emphasis = crate::diff::pair(old, new, hunk);
        if keep {
            kept.push(emphasis);
        }
    }
    kept
}

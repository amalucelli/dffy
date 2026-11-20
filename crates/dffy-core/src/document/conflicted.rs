//! A conflicted file as a [`File`] shows it: its line diff with the regions
//! its markers bound, or with choices made, the preview of what they
//! resolve it to. Either way the regions are where a host's row is, and a
//! region is where the host's view goes.

use std::ops::Range;
use std::sync::Arc;

use super::{File, Prepare, Side};
use crate::conflict::{self, Choices, Conflicts};
use crate::diff::{LineKind, Whitespace};
use crate::git::{FileDiff, Side as GitSide};

/// Lines of context around each region in a preview.
const CONTEXT: usize = 3;

/// A conflicted file's conflicts, as the diff loaded for it shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflicted {
    conflicts: Conflicts,
    /// The choices this diff previews, for a preview.
    choices: Option<Arc<Choices>>,
    /// Each region's lines on the diff's new side: the conflicted file's in
    /// the line diff, what each became in a preview.
    shown: Vec<Range<usize>>,
}

impl Conflicted {
    /// The regions, by line of the conflicted file.
    #[must_use]
    pub fn regions(&self) -> &[conflict::Region] {
        &self.conflicts.regions
    }

    /// Why the file can't be resolved by region, if it can't.
    #[must_use]
    pub fn refused(&self) -> Option<&str> {
        self.conflicts.refused.as_deref()
    }

    /// The choices previewed, for a preview; `None` for the line diff.
    #[must_use]
    pub fn choices(&self) -> Option<&Arc<Choices>> {
        self.choices.as_ref()
    }

    /// The region a row showing old line `old` and new line `new` (0-based,
    /// see [`super::Document::row_lines`]) is in.
    #[must_use]
    pub fn region_at(&self, [old, new]: [Option<usize>; 2]) -> Option<usize> {
        let within = |r: &Range<usize>, l: Option<usize>| l.is_some_and(|l| r.contains(&l));
        (0..self.shown.len()).find(|&i| {
            within(&self.shown[i], new)
                || (self.choices.is_some() && within(&self.conflicts.regions[i].lines(), old))
        })
    }

    /// The side and line a view on region `region` puts at its top: its
    /// first line, or in a preview the first it became, or its `<<<<<<<`
    /// line when it became nothing.
    fn anchor(&self, region: usize) -> Option<(Side, usize)> {
        let shown = self.shown.get(region)?;
        let chosen = self
            .choices
            .as_ref()
            .is_some_and(|c| c.picks.get(region).copied().flatten().is_some());
        Some(if chosen {
            (Side::Old, self.conflicts.regions[region].start)
        } else {
            (Side::New, shown.start)
        })
    }
}

impl File {
    /// The conflicts of a conflicted file whose new side is its
    /// working-tree file; `None` for any other.
    #[inline]
    #[must_use]
    pub fn conflicted(&self) -> Option<&Conflicted> {
        self.conflicted.as_deref()
    }

    /// A preview of choices on a conflicted file, rather than its diff.
    #[inline]
    #[must_use]
    pub fn is_resolution(&self) -> bool {
        self.conflicted
            .as_ref()
            .is_some_and(|c| c.choices.is_some())
    }

    /// The row a view on region `region` puts at its top (see
    /// [`Conflicted`]); `None` without such a region.
    pub(super) fn conflict_match(&self, region: usize) -> Option<super::Match> {
        let (side, line) = self.conflicted()?.anchor(region)?;
        Some(super::Match {
            side,
            line: u32::try_from(line).ok()?,
            start: 0,
            end: 0,
        })
    }
}

impl Prepare<'_> {
    /// A conflicted file prepared for display: `diff`, its line diff whose
    /// new side is the working-tree file, with `conflicts` (see
    /// [`crate::git::Repo::conflicts`]); or when `choices` fit those
    /// conflicts and have a choice, the preview of what they resolve it to.
    /// The preview is the diff from the working-tree file to the resolved
    /// one, with a hunk per region (see [`conflict::preview_hunks`]); its
    /// counts are the line diff's, so a file's counts stay as they were
    /// while choices are made.
    #[must_use]
    pub fn conflicted(
        &self,
        diff: FileDiff,
        conflicts: Conflicts,
        choices: Option<Arc<Choices>>,
        whitespace: Whitespace,
        highlight: bool,
    ) -> File {
        let choices = choices.filter(|c| c.regions == conflicts.regions && c.chosen() > 0);
        let Some(choices) = choices else {
            let mut file = self.file(diff, whitespace, highlight);
            let shown = conflicts
                .regions
                .iter()
                .map(conflict::Region::lines)
                .collect();
            file.conflicted = Some(Box::new(Conflicted {
                conflicts,
                choices: None,
                shown,
            }));
            return file;
        };
        let resolved = conflict::resolve(&diff.new, &choices.regions, &choices.picks);
        let hunks = conflict::preview_hunks(
            &diff.new,
            &resolved,
            &choices.regions,
            &choices.picks,
            CONTEXT,
        );
        let mut counts = super::Counts::default();
        for line in diff.hunks.iter().flat_map(|h| &h.lines) {
            match line.kind {
                LineKind::Added => counts.added += 1,
                LineKind::Removed => counts.removed += 1,
                LineKind::Context => {}
            }
        }
        // Both sides are the working-tree file's: nothing is cached under
        // an object id that isn't theirs.
        let worktree = diff.change.new.map(|side| match side {
            GitSide::Worktree { mode, .. } => GitSide::Worktree { mode, id: None },
            other => other,
        });
        let mut change = diff.change;
        change.old = worktree;
        change.new = worktree;
        let preview = FileDiff {
            change,
            old: diff.new,
            new: resolved.text,
            binary: false,
            too_large: false,
            note: None,
            hunks,
        };
        let mut file = self.file(preview, Whitespace::Exact, highlight);
        file.counts = counts;
        file.conflicted = Some(Box::new(Conflicted {
            conflicts,
            choices: Some(choices),
            shown: resolved.at,
        }));
        file
    }
}

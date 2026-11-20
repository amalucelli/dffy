//! Merge conflicts in a conflicted file: the regions git's conflict markers
//! bound in the working-tree file, a choice per region, and the file a set
//! of choices resolves to. A frontend shows the regions and takes the
//! choices; what a region is, what each choice keeps and the text it
//! writes are decided here, once, for every frontend.
//!
//! Regions are read from the markers, as the file is what gets written.
//! The index's sides of the conflict (see [`crate::git::Repo::conflicts`])
//! settle a region whose markers read more than one way, as when a side's
//! own code has a line of seven `=`.

use std::ops::Range;

use crate::diff::{Hunk, Line, LineKind, Text, Unchanged};

/// Characters in a conflict marker: git's default `conflict-marker-size`.
/// A file whose markers are longer, by that attribute, has no regions here.
const MARKER_SIZE: usize = 7;

/// A conflict marker line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// `<<<<<<<`, before the current side (ours).
    Start,
    /// `|||||||`, before the common ancestor's lines (diff3 and zdiff3).
    Base,
    /// `=======`, before the incoming side (theirs).
    Separator,
    /// `>>>>>>>`, after the incoming side.
    End,
}

/// The marker `line` is: seven `<`, `|`, `=` or `>`, then the end of the
/// line or a space and a label; `=======` takes no label. A `\r` before the
/// newline doesn't count, so a CRLF file's markers read the same.
#[must_use]
pub fn marker(line: &[u8]) -> Option<Marker> {
    let line = bare(line);
    let marker = match line.first()? {
        b'<' => Marker::Start,
        b'|' => Marker::Base,
        b'=' => Marker::Separator,
        b'>' => Marker::End,
        _ => return None,
    };
    let run = line.get(..MARKER_SIZE)?;
    if run.iter().any(|&b| b != line[0]) {
        return None;
    }
    match line.get(MARKER_SIZE) {
        None => Some(marker),
        Some(b' ') if marker != Marker::Separator => Some(marker),
        Some(_) => None,
    }
}

/// One conflict, by 0-based line of the conflicted file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    /// The `<<<<<<<` line.
    pub start: usize,
    /// The current side's lines, after it.
    pub ours: Range<usize>,
    /// The common ancestor's lines after `|||||||`, when git wrote them
    /// (`merge.conflictStyle` diff3 or zdiff3).
    pub base: Option<Range<usize>>,
    /// The incoming side's lines, after `=======`.
    pub theirs: Range<usize>,
    /// The `>>>>>>>` line.
    pub end: usize,
}

impl Region {
    /// Every line of it, markers included.
    #[must_use]
    pub fn lines(&self) -> Range<usize> {
        self.start..self.end + 1
    }

    /// The lines `choice` keeps, in order: one range, or two for
    /// [`Choice::Both`]. `None` for [`Choice::Base`] without a base.
    #[must_use]
    pub fn kept(&self, choice: Choice) -> Option<[Range<usize>; 2]> {
        let none = self.theirs.end..self.theirs.end;
        Some(match choice {
            Choice::Ours => [self.ours.clone(), none],
            Choice::Theirs => [self.theirs.clone(), none],
            Choice::Both => [self.ours.clone(), self.theirs.clone()],
            Choice::Base => [self.base.clone()?, none],
        })
    }
}

/// What a region resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Choice {
    /// The current side, the branch merged into (ours, `HEAD`).
    Ours,
    /// The incoming side (theirs).
    Theirs,
    /// The current side's lines, then the incoming side's.
    Both,
    /// The common ancestor's lines, when the markers have them.
    Base,
}

/// A conflicted file's conflicts, as [`crate::git::Repo::conflicts`] reads
/// them from the index and the working-tree file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Conflicts {
    /// The working-tree file's regions, in order.
    pub regions: Vec<Region>,
    /// Why the file can't be resolved by region (binary, deleted on one
    /// side, a symbolic link), in words for the user; no regions then.
    pub refused: Option<String>,
}

/// Choices made on a conflicted file, one per region of the text they
/// were made on, which they only fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choices {
    /// The regions chosen on.
    pub regions: Vec<Region>,
    /// Per region, `None` while it has no choice.
    pub picks: Vec<Option<Choice>>,
}

impl Choices {
    /// None yet, for `regions`.
    #[must_use]
    pub fn new(regions: Vec<Region>) -> Self {
        let picks = vec![None; regions.len()];
        Self { regions, picks }
    }

    /// Regions with a choice.
    #[must_use]
    pub fn chosen(&self) -> usize {
        self.picks.iter().flatten().count()
    }
}

/// The conflict's sides as the index has them (stages 1, 2 and 3), to
/// settle regions whose markers read more than one way. A side not there,
/// or not read, isn't checked.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sides<'a> {
    /// The common ancestor (stage 1).
    pub base: Option<&'a Text>,
    /// The current side (stage 2).
    pub ours: Option<&'a Text>,
    /// The incoming side (stage 3).
    pub theirs: Option<&'a Text>,
}

/// The conflict regions of `text`, in order. A marker line outside a
/// region, or a `<<<<<<<` that no `=======` and `>>>>>>>` follow, is text.
/// Where a side's own lines look like markers, the first reading that
/// `sides` holds each side of, as a run of lines, wins; with none, the
/// first reading: the earliest `=======`, then the earliest `>>>>>>>`.
#[must_use]
pub fn regions(text: &Text, sides: &Sides<'_>) -> Vec<Region> {
    let markers: Vec<Option<Marker>> = text.lines().map(marker).collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < markers.len() {
        if markers[at] != Some(Marker::Start) {
            at += 1;
            continue;
        }
        let window = at + 1
            ..(at + 1..markers.len())
                .find(|&i| markers[i] == Some(Marker::Start))
                .unwrap_or(markers.len());
        let mut readings = readings(at, &markers[window.clone()], window.start).peekable();
        let first = readings.peek().cloned();
        match readings.find(|r| holds(text, r, sides)).or(first) {
            Some(region) => {
                at = region.end + 1;
                out.push(region);
            }
            None => at = window.end,
        }
    }
    out
}

/// The regions starting at `start` that the markers in `window` (whose
/// first line is `offset`) can bound, the first reading first.
fn readings(
    start: usize,
    window: &[Option<Marker>],
    offset: usize,
) -> impl Iterator<Item = Region> + '_ {
    let at = move |kind: Marker| {
        window
            .iter()
            .enumerate()
            .filter(move |(_, m)| **m == Some(kind))
            .map(move |(i, _)| i + offset)
    };
    at(Marker::End).flat_map(move |end| {
        at(Marker::Separator)
            .take_while(move |&sep| sep < end)
            .flat_map(move |sep| {
                let bases = at(Marker::Base).take_while(move |&b| b < sep);
                bases.map(Some).chain([None]).map(move |base| Region {
                    start,
                    ours: start + 1..base.unwrap_or(sep),
                    base: base.map(|b| b + 1..sep),
                    theirs: sep + 1..end,
                    end,
                })
            })
    })
}

/// Whether each side of `region` is a run of lines of that side in
/// `sides`, line endings aside.
fn holds(text: &Text, region: &Region, sides: &Sides<'_>) -> bool {
    let has = |side: Option<&Text>, lines: Option<&Range<usize>>| match (side, lines) {
        (Some(side), Some(lines)) => has_run(side, text, lines),
        (None, _) | (_, None) => true,
    };
    has(sides.ours, Some(&region.ours))
        && has(sides.theirs, Some(&region.theirs))
        && has(sides.base, region.base.as_ref())
}

/// Whether `lines` of `text` show up, in order and together, in `side`.
// O(lines × side), fine for the few readings of the one file resolved.
fn has_run(side: &Text, text: &Text, lines: &Range<usize>) -> bool {
    let n = lines.len();
    n == 0
        || (0..(side.len() + 1).saturating_sub(n))
            .any(|i| (0..n).all(|k| bare(side.line(i + k)) == bare(text.line(lines.start + k))))
}

/// `line` without its line ending.
fn bare(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// A conflicted file resolved by a set of choices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The file's text.
    pub text: Text,
    /// Each region's lines in it: what its choice kept, or the region as it
    /// was, markers and all, without one.
    pub at: Vec<Range<usize>>,
}

/// What `choices` make of `text`, whose regions are `regions`: one choice
/// per region, `None` leaving it as it is. Lines outside the regions stay
/// byte for byte, line endings included. When the last region ends the
/// file and its `>>>>>>>` line has no newline, neither has the resolved
/// file's last line.
#[must_use]
pub fn resolve(text: &Text, regions: &[Region], choices: &[Option<Choice>]) -> Resolved {
    let mut bytes = Vec::with_capacity(text.bytes().len());
    let mut at = Vec::with_capacity(regions.len());
    let (mut line, mut lines) = (0, 0);
    let copy = |range: Range<usize>, bytes: &mut Vec<u8>| {
        bytes.extend_from_slice(&text.bytes()[text.offset(range.start)..text.offset(range.end)]);
        range.len()
    };
    for (i, region) in regions.iter().enumerate() {
        lines += copy(line..region.start, &mut bytes);
        let kept = choices
            .get(i)
            .copied()
            .flatten()
            .and_then(|c| region.kept(c))
            .unwrap_or_else(|| [region.lines(), region.end + 1..region.end + 1]);
        let first = lines;
        for range in kept {
            lines += copy(range, &mut bytes);
        }
        let ends_file = region.end + 1 == text.len() && !text.line(region.end).ends_with(b"\n");
        if ends_file && lines > first {
            let ending = if bytes.ends_with(b"\r\n") {
                2
            } else {
                usize::from(bytes.ends_with(b"\n"))
            };
            bytes.truncate(bytes.len() - ending);
        }
        at.push(first..lines);
        line = region.end + 1;
    }
    copy(line..text.len(), &mut bytes);
    Resolved {
        text: Text::new(bytes),
        at,
    }
}

/// The diff from `text`, the conflicted file, to `resolved`, what `choices`
/// make of it: a hunk per region with up to `context` lines around it,
/// regions whose context meets sharing one. A chosen region's lines are
/// removed and the lines its choice kept added; one without a choice shows
/// unchanged, so every region shows, chosen or not.
#[must_use]
pub fn preview_hunks(
    text: &Text,
    resolved: &Resolved,
    regions: &[Region],
    choices: &[Option<Choice>],
    context: usize,
) -> Vec<Hunk> {
    let line = |kind, old, new| Line {
        kind,
        old,
        new,
        emphasis: Vec::new(),
        unchanged: Unchanged::No,
        block_start: false,
        paired: false,
    };
    let mut hunks = Vec::new();
    // New lines minus old ones so far: outside the regions, old line `o` is
    // new line `o + shift`.
    let mut shift: isize = 0;
    let mut i = 0;
    while i < regions.len() {
        let lead = regions[i].start.saturating_sub(context);
        let new_start = lead.saturating_add_signed(shift);
        let mut lines: Vec<Line> = (lead..regions[i].start)
            .map(|o| line(LineKind::Context, o, o.saturating_add_signed(shift)))
            .collect();
        loop {
            let (region, to) = (&regions[i], &resolved.at[i]);
            if choices.get(i).copied().flatten().is_some() {
                lines.extend(region.lines().map(|o| line(LineKind::Removed, o, to.start)));
                lines.extend(to.clone().map(|n| line(LineKind::Added, region.end + 1, n)));
            } else {
                lines.extend(
                    region
                        .lines()
                        .zip(to.clone())
                        .map(|(o, n)| line(LineKind::Context, o, n)),
                );
            }
            shift += isize::try_from(to.len()).unwrap_or(0)
                - isize::try_from(region.lines().len()).unwrap_or(0);
            i += 1;
            let after = region.end + 1;
            let next = regions
                .get(i)
                .filter(|next| next.start.saturating_sub(context) <= after + context);
            let until = next.map_or_else(|| (after + context).min(text.len()), |next| next.start);
            lines.extend(
                (after..until).map(|o| line(LineKind::Context, o, o.saturating_add_signed(shift))),
            );
            if next.is_none() {
                break;
            }
        }
        let count = |skip: LineKind| lines.iter().filter(|l| l.kind != skip).count();
        hunks.push(Hunk {
            old_start: lead,
            old_len: count(LineKind::Added),
            new_start,
            new_len: count(LineKind::Removed),
            lines,
        });
    }
    hunks
}

#[cfg(test)]
mod tests;

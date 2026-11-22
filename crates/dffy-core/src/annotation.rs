//! Annotations: ranges of a file's lines that something outside the diff
//! holds on to, each with the text it was made on, and where they land on a
//! version of the file. An annotation records its lines as they were; when
//! the file changed since, its anchor text finds them again, or tells they
//! are gone. What an annotation says and how it's drawn is the host's: here
//! it's an id and the lines it's on. A suggestion made on anchored lines is
//! applied to the working tree through the same placement.
//!
//! Placement reads only the two sides of a file's diff, so a host runs it
//! off its main thread with the diff it loads. [`AnnotationSet`] stamps
//! each path with the generation its annotations last changed at, so a
//! placement made before a change is told apart from a current one, and a
//! change to one path re-places that path alone.
//!
//! Lines are 0-based lines of a side, as [`crate::diff::Text`] has them.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gix::bstr::BString;

use crate::diff::Text;
use crate::document::Side;
use crate::git::{self, FileDiff, NewSide, Repo};

/// Lines of context an anchor keeps on each side of the anchored lines.
const CONTEXT: usize = 2;

/// How far from its recorded first line a moved anchor is looked for.
const NEARBY: usize = 1000;

/// Lines shown around a placed annotation that a gap hid.
pub(crate) const REVEAL_CONTEXT: usize = 2;

/// The anchored lines' text and up to two lines around them, each without
/// its `\n` and decoded lossily.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Anchor {
    /// The lines just above, nearest last.
    pub before: Vec<String>,
    /// The anchored lines.
    pub lines: Vec<String>,
    /// The lines just below, nearest first.
    pub after: Vec<String>,
}

impl Anchor {
    /// The lines `range` of `text`, a non-empty range within it, with their
    /// context.
    #[must_use]
    pub fn new(text: &Text, range: Range<usize>) -> Self {
        let line = |i| String::from_utf8_lossy(without_newline(text.line(i))).into_owned();
        Self {
            before: (range.start.saturating_sub(CONTEXT)..range.start)
                .map(line)
                .collect(),
            after: (range.end..(range.end + CONTEXT).min(text.len()))
                .map(line)
                .collect(),
            lines: range.map(line).collect(),
        }
    }

    /// Where the anchored lines are in `text`, recorded as starting at line
    /// `at`: there when they still match, else wherever the lines and their
    /// context appear exactly once within 1000 lines of it. `None` when
    /// they're gone, found more than once, or there are none to find.
    #[must_use]
    pub fn place(&self, text: &Text, at: usize) -> Option<Range<usize>> {
        self.find(text, at).ok()
    }

    /// [`Anchor::place`], with why nothing was found.
    fn find(&self, text: &Text, at: usize) -> Result<Range<usize>, Missing> {
        let len = self.lines.len();
        if len == 0 {
            return Err(Missing::Gone);
        }
        if matches_at(text, at, &self.lines) {
            return Ok(at..at + len);
        }
        let mut found = None;
        for first in at.saturating_sub(NEARBY)..=at.saturating_add(NEARBY).min(text.len()) {
            let Some(before) = first.checked_sub(self.before.len()) else {
                continue;
            };
            if matches_at(text, before, &self.before)
                && matches_at(text, first, &self.lines)
                && matches_at(text, first + len, &self.after)
            {
                if found.is_some() {
                    return Err(Missing::Repeated);
                }
                found = Some(first..first + len);
            }
        }
        found.ok_or(Missing::Gone)
    }
}

/// Why an anchor's lines weren't placed.
enum Missing {
    Gone,
    /// Found more than once away from where they were recorded.
    Repeated,
}

/// Why no suggestion can apply to `shown`'s file, from what the diff
/// already says: no I/O, for a frontend to ask before it offers one.
///
/// # Errors
///
/// [`git::Error::Refused`] for a diff whose new side isn't the working
/// tree, a file deleted there, symbolic links and submodules, binary files,
/// files too large to diff and files shown as read from disk because their
/// filter failed. A file plain `dffy` shows staged, whose new side is the
/// index, counts as the working tree: they match. So does a blob of a
/// commit's tree with [`NewSide::Head`].
pub fn check_suggestion(shown: &FileDiff, new_side: NewSide) -> git::Result<()> {
    git::check_suggestion(shown, new_side)
}

/// What [`apply_suggestion`] wrote, in 0-based lines of the file as git
/// reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Where the anchored lines were, before the write.
    pub replaced: Range<usize>,
    /// Where the replacement is, after it: empty where lines were deleted.
    pub lines: Range<usize>,
    /// `lines` anchored in the file as written, to hold on to them; `None`
    /// when they're empty.
    pub anchor: Option<Anchor>,
}

/// Puts `replacement`'s lines in place of the lines `anchor` holds, in the
/// working-tree file that `shown` is a diff of, and returns where they
/// were and where the new ones are.
///
/// `new_side` says whether `shown`'s new side may be a commit's tree: with
/// [`NewSide::Head`], the caller checked it's HEAD's version of the file
/// ([`Repo::head_blob`]), so the working tree was made from it. The lines are found in the file on disk
/// either way, and refused when they no longer match.
///
/// `at` is the first anchored line as recorded on `shown`'s new side. The
/// file is read now and the lines placed on it as [`Anchor::place`] does:
/// at `at` when they still match, else where they and their context appear
/// once nearby. `replacement` holds lines without their endings; none
/// removes the anchored lines.
///
/// Only the working tree is written, never the index or a commit, so the
/// result is an unstaged change. The new lines take the line ending of
/// those they replace, a missing newline at the end of the file stays
/// missing, and the rest of the file is kept byte for byte. The file is
/// replaced through a rename in its directory and keeps its permissions,
/// but it's a new file: another hard link to the old one keeps the old
/// text, and its extended attributes, ACLs and owner aren't carried over.
/// The file isn't locked: an edit saved between the read and the rename
/// is lost.
///
/// # Errors
///
/// [`git::Error::Refused`], writing nothing: as [`check_suggestion`] says;
/// when the file is gone, binary, or converted by git other than in its
/// line endings (a clean or process filter such as LFS, `ident`, an
/// encoding); when the anchored lines changed since, or moved and are
/// found more than once. [`git::Error::Io`] when the file can't be read or
/// replaced.
pub fn apply_suggestion(
    repo: &Repo,
    shown: &FileDiff,
    new_side: NewSide,
    at: usize,
    anchor: &Anchor,
    replacement: &[&str],
) -> git::Result<Applied> {
    let find = |text: &Text| {
        anchor.find(text, at).map_err(|missing| {
            git::Error::Refused(
                match missing {
                    Missing::Gone => {
                        "can't apply the suggestion: its lines changed since it was written"
                    }
                    Missing::Repeated => {
                        "can't apply the suggestion: its lines moved and are in the file \
                         more than once"
                    }
                }
                .to_owned(),
            )
        })
    };
    let (replaced, after) = repo.replace_lines(shown, new_side, find, replacement)?;
    let lines = replaced.start..replaced.start + replacement.len();
    Ok(Applied {
        anchor: (!lines.is_empty()).then(|| Anchor::new(&after, lines.clone())),
        replaced,
        lines,
    })
}

fn matches_at(text: &Text, at: usize, lines: &[String]) -> bool {
    at + lines.len() <= text.len()
        && lines.iter().enumerate().all(|(k, want)| {
            let line = without_newline(text.line(at + k));
            line == want.as_bytes()
                || (std::str::from_utf8(line).is_err() && String::from_utf8_lossy(line) == **want)
        })
}

fn without_newline(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n").unwrap_or(line)
}

/// A range of lines of one side of a file, as recorded when it was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    /// The host's; unique within an [`AnnotationSet`].
    pub id: String,
    /// What made it, named by the host (`comments` for review comments):
    /// a frontend that draws sources differently tells them apart by it,
    /// never by their ids.
    pub source: &'static str,
    /// The file the lines are in.
    pub path: BString,
    /// Where the old side's lines are when the file was renamed: an
    /// old-side annotation also sits on this path's diff, for a list that
    /// shows the rename as a deletion and an addition.
    pub old_path: Option<BString>,
    /// The side `lines` are on.
    pub side: Side,
    /// Non-empty.
    pub lines: Range<usize>,
    /// Without one, the annotation stays at `lines` while the side still
    /// has them.
    pub anchor: Option<Anchor>,
}

/// Where an annotation landed on a version of its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// Its lines are found.
    Placed {
        /// The annotation's.
        id: String,
        /// Its side.
        side: Side,
        /// Where its lines are now; non-empty.
        lines: Range<usize>,
    },
    /// Its lines aren't in this version, or can't be told apart.
    Lost {
        /// The annotation's.
        id: String,
    },
}

impl Placement {
    /// The id of the annotation placed.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Placed { id, .. } | Self::Lost { id } => id,
        }
    }
}

/// Places `annotations` on the two sides of `diff`, in order.
#[must_use]
pub fn place(annotations: &[Annotation], diff: &FileDiff) -> Vec<Placement> {
    annotations
        .iter()
        .map(|a| {
            let text = match a.side {
                Side::Old => &diff.old,
                Side::New => &diff.new,
            };
            let found = match &a.anchor {
                Some(anchor) => anchor.place(text, a.lines.start),
                None => (a.lines.end <= text.len()).then(|| a.lines.clone()),
            };
            let id = a.id.clone();
            match found {
                Some(lines) => Placement::Placed {
                    id,
                    side: a.side,
                    lines,
                },
                None => Placement::Lost { id },
            }
        })
        .collect()
}

/// The annotations on one path, and the generation they last changed at.
#[derive(Debug, Clone)]
pub struct Annotated {
    /// A placement made at this generation or later is current.
    pub generation: u64,
    /// In the order they were given.
    pub annotations: Arc<[Annotation]>,
}

/// Generations are unique in the process, so a placement made for one set
/// is never taken as current by another, as when a host's source of
/// annotations is replaced while a placement for the old one runs.
static GENERATIONS: AtomicU64 = AtomicU64::new(0);

/// Every annotation a host holds, by the path whose diff they sit on.
/// Cloning shares the annotations: a host hands a copy to its workers.
#[derive(Debug, Default, Clone)]
pub struct AnnotationSet {
    generation: u64,
    paths: HashMap<BString, Annotated>,
}

impl AnnotationSet {
    /// Moves on with every change to the annotations.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Takes `annotations` in place of the ones held. Returns the paths
    /// whose annotations changed, sorted; only those get a new generation.
    pub fn replace(&mut self, annotations: impl IntoIterator<Item = Annotation>) -> Vec<BString> {
        let mut by_path: HashMap<BString, Vec<Annotation>> = HashMap::new();
        for a in annotations {
            if let Some(old) =
                (a.old_path.as_ref()).filter(|old| a.side == Side::Old && **old != a.path)
            {
                by_path.entry(old.clone()).or_default().push(a.clone());
            }
            by_path.entry(a.path.clone()).or_default().push(a);
        }
        let mut changed: Vec<BString> = (self.paths.keys())
            .filter(|p| !by_path.contains_key(*p))
            .cloned()
            .collect();
        let next = GENERATIONS.fetch_add(1, Ordering::Relaxed) + 1;
        let mut paths = HashMap::with_capacity(by_path.len());
        for (path, annotations) in by_path {
            let kept = (self.paths.remove(&path)).filter(|a| *a.annotations == *annotations);
            let annotated = kept.unwrap_or_else(|| {
                changed.push(path.clone());
                Annotated {
                    generation: next,
                    annotations: annotations.into(),
                }
            });
            paths.insert(path, annotated);
        }
        self.paths = paths;
        if !changed.is_empty() {
            self.generation = next;
        }
        changed.sort_unstable();
        changed
    }

    /// The annotations on `path`'s diff: its own, and the old side's of a
    /// file renamed from it.
    #[must_use]
    pub fn on(&self, path: &[u8]) -> Option<&Annotated> {
        self.paths.get(path)
    }

    /// Whether a placement on `path` made at `generation` still holds.
    #[must_use]
    pub fn is_current(&self, path: &[u8], generation: u64) -> bool {
        self.paths
            .get(path)
            .is_none_or(|a| generation >= a.generation)
    }
}

#[cfg(test)]
mod tests;

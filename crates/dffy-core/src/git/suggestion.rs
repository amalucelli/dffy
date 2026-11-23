//! The working tree's half of applying a review suggestion: replacing lines
//! of a file on disk, never the index. Finding the lines is the annotation
//! layer's ([`crate::annotation::apply_suggestion`]); here they're found on
//! the file as git reads it, the text its diff shows, and replaced in the
//! bytes on disk, so line endings and every other line stay as they were.

use std::io::ErrorKind;
use std::ops::Range;

use gix::bstr::ByteSlice;

use super::{Error, FileDiff, Mode, Repo, Result, Side, is_binary, worktree};
use crate::diff::Text;
use crate::view::Staging;

fn refuse(why: &str) -> Error {
    Error::Refused(format!("can't apply a suggestion {why}"))
}

/// What the new side of the diff a suggestion is applied on is, past what
/// the diff itself says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewSide {
    /// Only what the diff says: the working tree, or the index for a file
    /// plain `dffy` shows staged, which the working tree matches. A
    /// commit's tree is refused.
    AsShown,
    /// A blob the caller checked is HEAD's version of the file, as on a
    /// range ending at HEAD or a review's head once it's checked out: the
    /// working tree was made from it.
    Head,
}

/// Why no suggestion can apply to `diff`'s file, from what the diff says.
pub(crate) fn check_suggestion(diff: &FileDiff, new_side: NewSide) -> Result<()> {
    let change = &diff.change;
    let Some(new) = change.new else {
        return Err(refuse("to a file that isn't in the working tree"));
    };
    // Plain `dffy` shows a staged file's index side; the working tree
    // matches it.
    let worktree = matches!(new, Side::Worktree { .. })
        || change.staging == Some(Staging::Staged)
        || (new_side == NewSide::Head && matches!(new, Side::Object { .. }));
    if !worktree {
        return Err(Error::Refused(
            "can't apply a suggestion: it works on the working tree or HEAD, \
             and this file's new side is neither"
                .to_owned(),
        ));
    }
    if !matches!(new.mode(), Mode::File | Mode::Executable) {
        return Err(refuse("to a symbolic link or a submodule"));
    }
    if diff.binary {
        return Err(refuse("to a binary file"));
    }
    if diff.too_large {
        return Err(refuse("to a file too large to diff"));
    }
    if diff.note.is_some() {
        return Err(refuse(
            "to a file whose filter failed: it's shown as on disk",
        ));
    }
    Ok(())
}

impl Repo {
    /// Puts the `replacement` lines in place of those `find` picks from the
    /// working-tree file of `shown`, as git reads it, and returns them with
    /// the file as git reads it after.
    pub(crate) fn replace_lines(
        &self,
        shown: &FileDiff,
        new_side: NewSide,
        find: impl FnOnce(&Text) -> Result<Range<usize>>,
        replacement: &[&str],
    ) -> Result<(Range<usize>, Text)> {
        check_suggestion(shown, new_side)?;
        let path = shown.change.path.as_bstr();
        let read = self.worktree_path(path).and_then(|full| {
            let io = |source| Error::Io {
                path: full.clone(),
                source,
            };
            let meta = std::fs::symlink_metadata(&full).map_err(io)?;
            // Renaming over a link would replace it with a file of its
            // target's text.
            if !meta.is_file() {
                return Err(refuse("to a symbolic link or a submodule"));
            }
            let raw = std::fs::read(&full).map_err(io)?;
            Ok((full, meta.permissions(), raw))
        });
        let (full, permissions, raw) = match read {
            Err(Error::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
                return Err(refuse("to a file that isn't in the working tree"));
            }
            read => read?,
        };
        if is_binary(&raw) {
            return Err(refuse("to a binary file"));
        }
        let mut converted = raw.clone();
        if let Some(note) = self.to_git(path, &mut converted) {
            return Err(refuse(&format!("to this file: {note}")));
        }
        let (raw, converted) = (Text::new(raw), Text::new(converted));
        if !same_but_line_endings(&raw, &converted) {
            return Err(refuse(
                "to a file git converts with a filter, ident or an encoding; edit it directly",
            ));
        }
        let lines = find(&converted)?;
        let bytes = splice(&raw, lines.clone(), replacement);
        worktree::replace(&full, &bytes, permissions)
            .map_err(|source| Error::Io { path: full, source })?;
        let after = Text::new(splice(&converted, lines.clone(), replacement));
        Ok((lines, after))
    }
}

/// Whether `converted`, the to-git form of `raw`, differs from it only in
/// line endings, so that a line of one is the same line of the other.
fn same_but_line_endings(raw: &Text, converted: &Text) -> bool {
    fn content(line: &[u8]) -> &[u8] {
        match line.strip_suffix(b"\n") {
            Some(line) => line.strip_suffix(b"\r").unwrap_or(line),
            None => line,
        }
    }
    raw.bytes() == converted.bytes()
        || (raw.len() == converted.len()
            && (raw.lines().zip(converted.lines())).all(|(a, b)| content(a) == content(b)))
}

/// `raw` with `lines` replaced by the `replacement` lines, each ended as
/// the first of `lines` that has an ending (else the line above). A missing
/// newline at the end of the file stays missing.
fn splice(raw: &Text, lines: Range<usize>, replacement: &[&str]) -> Vec<u8> {
    let ending = |i: usize| -> Option<&'static [u8]> {
        let line = raw.line(i);
        if line.ends_with(b"\r\n") {
            Some(b"\r\n")
        } else if line.ends_with(b"\n") {
            Some(b"\n")
        } else {
            None
        }
    };
    let eol = (lines.clone().chain(lines.start.checked_sub(1)))
        .find_map(ending)
        .unwrap_or(b"\n");
    let open_end = lines.end == raw.len() && !raw.is_empty() && ending(raw.len() - 1).is_none();
    let mut out = raw.bytes()[..raw.offset(lines.start)].to_vec();
    // The ending `out` closes with: the line above's, then each new line's.
    let mut last = lines.start.checked_sub(1).and_then(ending);
    for line in replacement {
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(eol);
        last = Some(eol);
    }
    if open_end && let Some(last) = last {
        out.truncate(out.len() - last.len());
    }
    out.extend_from_slice(&raw.bytes()[raw.offset(lines.end)..]);
    out
}

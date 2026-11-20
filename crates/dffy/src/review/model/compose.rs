//! Writing a comment: a new one's lines, numbers and anchor taken from the
//! diff, and the text box's or the editor's text turned into the operation
//! that saves it, a suggestion in a closing fenced block included.

use std::ops::Range;

use dffy_core::annotation::Anchor;
use dffy_core::diff::{Hunk, LineKind};
use dffy_core::document::Side;
use dffy_core::git::{FileChange, FileDiff, Status};

use crate::review::{self, Comment, LineNumbers, Op};

/// What the text box's or the editor's text becomes, kept while either is
/// open.
#[derive(Clone)]
pub(crate) enum Compose {
    New(Box<Comment>),
    /// A reply to the thread with this root.
    Reply(String),
    Edit {
        id: String,
        before: String,
    },
}

impl Compose {
    /// The operation saving `text`, and what the status bar says after;
    /// `None` when there's nothing to save. A draft edited down to nothing
    /// is deleted, as `d d` would. A comment on lines that ends in a
    /// `suggestion` fenced block, as [`with_suggestion`] writes it, keeps
    /// the block's lines as its suggestion, as they were typed; a new one
    /// suggesting the lines as they are, with nothing said, is nothing.
    pub(crate) fn into_op(self, text: &str) -> Option<(Op, &'static str)> {
        let lines = match &self {
            Self::New(comment) => comment.end.is_some(),
            Self::Edit { .. } => true,
            Self::Reply(_) => false,
        };
        let (text, mut suggestion) = if lines {
            split_suggestion(text)
        } else {
            (text, None)
        };
        let text = text.trim();
        if let Self::New(comment) = &self
            && text.is_empty()
            && suggestion == comment.anchor.as_ref().map(|a| a.lines.join("\n"))
        {
            suggestion = None;
        }
        if text.is_empty() && suggestion.is_none() {
            return match self {
                Self::Edit { id, .. } => Some((Op::Rm(review::Target::Id(id)), "draft deleted")),
                Self::New(_) | Self::Reply(_) => None,
            };
        }
        Some(match self {
            Self::New(mut comment) => {
                text.clone_into(&mut comment.text);
                comment.suggestion = suggestion;
                (
                    Op::Add {
                        comment,
                        reply_to: None,
                    },
                    "comment saved",
                )
            }
            Self::Reply(root) => (
                Op::Add {
                    comment: Box::new(Comment {
                        text: text.to_owned(),
                        ..Comment::default()
                    }),
                    reply_to: Some(review::Target::Id(root)),
                },
                "reply saved",
            ),
            Self::Edit { before, .. }
                if split_suggestion(&before) == (text, suggestion.clone()) =>
            {
                return None;
            }
            Self::Edit { id, .. } => (
                Op::Set {
                    target: review::Target::Id(id),
                    change: review::Change {
                        text: Some(text.to_owned()),
                        suggestion: Some(suggestion),
                        ..review::Change::default()
                    },
                },
                "comment saved",
            ),
        })
    }
}

/// `text` with `suggestion` after it as a closing `suggestion` fenced
/// block, the way code hosts write one, to edit both in one box.
pub(crate) fn with_suggestion(text: &str, suggestion: Option<&str>) -> String {
    let Some(suggestion) = suggestion else {
        return text.to_owned();
    };
    let text = text.trim_end();
    let gap = if text.is_empty() { "" } else { "\n\n" };
    format!("{text}{gap}```suggestion\n{suggestion}\n```")
}

/// `text` without the closing `suggestion` fenced block [`with_suggestion`]
/// writes, trimmed, and the block's lines, without their last line break.
fn split_suggestion(text: &str) -> (&str, Option<String>) {
    text.trim_end()
        .strip_suffix("```")
        .and_then(|t| t.rsplit_once("```suggestion\n"))
        .filter(|(before, _)| before.is_empty() || before.ends_with('\n'))
        .map_or_else(
            || (text.trim(), None),
            |(before, block)| {
                let block = block.strip_suffix('\n').unwrap_or(block);
                (before.trim(), Some(block.to_owned()))
            },
        )
}

/// A new comment, without its text, on `lines` (0-based, non-empty) of
/// `side` of `diff`, the line diff of `change`.
pub(crate) fn draft(
    change: &FileChange,
    diff: &FileDiff,
    side: Side,
    lines: Range<usize>,
) -> Result<Comment, &'static str> {
    let (text, ours) = match side {
        Side::Old => (&diff.old, review::Side::Old),
        Side::New => (&diff.new, review::Side::New),
    };
    if lines.is_empty() || lines.end > text.len() {
        return Err("no line of code here to comment on");
    }
    Ok(Comment {
        side: ours,
        start: Some(numbers(&diff.hunks, side, lines.start)),
        end: Some(numbers(&diff.hunks, side, lines.end - 1)),
        anchor: Some(Anchor::new(text, lines.clone())),
        distance: review::distance(lines, ours, &diff.hunks),
        ..file_draft(change)?
    })
}

/// A new comment, without its text, on the whole file of `change`.
pub(crate) fn file_draft(change: &FileChange) -> Result<Comment, &'static str> {
    let utf8 = |path: &[u8]| {
        std::str::from_utf8(path)
            .map(str::to_owned)
            .map_err(|_| "the comments file holds UTF-8 paths only; this one isn't")
    };
    let old_path = match &change.status {
        Status::Renamed { from, .. } => Some(utf8(from)?),
        _ => None,
    };
    Ok(Comment {
        path: utf8(&change.path)?,
        old_path,
        ..Comment::default()
    })
}

/// Line `line` of `side`, with its number on the other side when it's on
/// both: from the hunk holding it, else shifted as the hunks above shift
/// lines.
fn numbers(hunks: &[Hunk], side: Side, line: usize) -> LineNumbers {
    let kind = match side {
        Side::Old => LineKind::Removed,
        Side::New => LineKind::Added,
    };
    let mine = |l: &dffy_core::diff::Line| match side {
        Side::Old => l.old,
        Side::New => l.new,
    };
    let theirs = |l: &dffy_core::diff::Line| match side {
        Side::Old => l.new,
        Side::New => l.old,
    };
    let mut other = Some(line);
    for hunk in hunks {
        let ((start, len), (other_start, other_len)) = match side {
            Side::Old => (
                (hunk.old_start, hunk.old_len),
                (hunk.new_start, hunk.new_len),
            ),
            Side::New => (
                (hunk.new_start, hunk.new_len),
                (hunk.old_start, hunk.old_len),
            ),
        };
        if line < start {
            break;
        }
        if line < start + len {
            other = hunk
                .lines
                .iter()
                .find(|l| mine(l) == line && (l.kind == LineKind::Context || l.kind == kind))
                .filter(|l| l.kind == LineKind::Context)
                .map(theirs);
            break;
        }
        other = Some(line - (start + len) + other_start + other_len);
    }
    match side {
        Side::Old => LineNumbers {
            old: Some(line),
            new: other,
        },
        Side::New => LineNumbers {
            old: other,
            new: Some(line),
        },
    }
}

#[cfg(test)]
mod tests {
    use dffy_core::diff::{Options, hunks};

    use super::super::tests::text;
    use super::*;

    /// A changed line has a number only on its own side; below a hunk, the
    /// other side's is shifted by what the hunk added.
    #[test]
    fn line_numbers_on_both_sides_for_unchanged_lines() {
        let old = text("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n");
        let new = text("a\nB\nB2\nc\nd\ne\nf\ng\nh\ni\nj\n");
        let hunks = hunks(
            &old,
            &new,
            &Options {
                context: 1,
                ..Options::default()
            },
        );
        let n = |side, line| {
            let n = numbers(&hunks, side, line);
            (n.old, n.new)
        };
        assert_eq!(n(Side::New, 0), (Some(0), Some(0)), "context in the hunk");
        assert_eq!(n(Side::New, 1), (None, Some(1)), "added");
        assert_eq!(n(Side::Old, 1), (Some(1), None), "removed");
        assert_eq!(n(Side::New, 9), (Some(8), Some(9)), "below the hunk");
        assert_eq!(n(Side::Old, 8), (Some(8), Some(9)));
    }
}

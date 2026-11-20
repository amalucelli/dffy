//! The comments file read once, for the frontends that never see it
//! change: printed output, `--json` and MCP.

use std::collections::{HashMap, HashSet};

use dffy_core::annotation::{Annotation, Placement};
use dffy_core::bstr::BString;
use dffy_core::document::Side;
use dffy_core::git::{FileChange, FileDiff};

use super::{At, annotation};
use crate::review::{self, Comment, Review};

/// A comments file's threads as read once, for the frontends that place
/// them on each file's diff as they make it and never see the file change:
/// printed output, `--json` and MCP. It's immutable, so the threads diffing
/// files share it. General comments aren't on a file, so it has none.
pub(crate) struct Snapshot {
    comments: Vec<Comment>,
    /// Indices into `comments` of the threads' first comments, in thread
    /// order, by the path whose diff shows them. A line comment on a
    /// renamed file's old side is also under its `old_path`, for a diff
    /// that lists the rename as a deletion and an addition.
    roots: HashMap<BString, Vec<usize>>,
    /// Indices of each thread's replies, in the order they were added, by
    /// its first comment's id.
    replies: HashMap<String, Vec<usize>>,
}

/// A thread placed on a file's diff by [`Snapshot::on`].
#[derive(Debug)]
pub(crate) struct Hung<'a> {
    /// Its first comment, then its replies in the order they were added.
    pub thread: Vec<&'a Comment>,
    /// [`At::File`], [`At::Line`] on the side shown, or [`At::Lost`].
    pub at: At,
}

impl Snapshot {
    /// The threads of `review` that `branches` keeps.
    pub(crate) fn new(review: &Review, branches: review::Branches) -> Self {
        let select = review::Select {
            branches,
            ..review::Select::default()
        };
        let comments: Vec<Comment> = review::threads(review.selected(&select))
            .into_iter()
            .filter(|c| !c.path.is_empty())
            .cloned()
            .collect();
        let ids: HashSet<String> = comments.iter().map(|c| c.id.clone()).collect();
        let mut roots: HashMap<BString, Vec<usize>> = HashMap::new();
        let mut replies: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, c) in comments.iter().enumerate() {
            if let Some(root) = c.in_reply_to.as_ref().filter(|r| ids.contains(*r)) {
                replies.entry(root.clone()).or_default().push(i);
                continue;
            }
            roots.entry(c.path.as_bytes().into()).or_default().push(i);
            if let (Some(old), Some((review::Side::Old, _))) = (&c.old_path, c.range())
                && *old != c.path
            {
                roots.entry(old.as_bytes().into()).or_default().push(i);
            }
        }
        Self {
            comments,
            roots,
            replies,
        }
    }

    /// The path `change` (reversed with `-R` when `reverse`) has its threads
    /// under, and whether its diff has the side the threads were written on
    /// as old.
    fn key(change: &FileChange, reverse: bool) -> (&[u8], bool) {
        if reverse {
            (change.old_path(), change.new.is_some())
        } else {
            (&change.path, change.old.is_some())
        }
    }

    /// Whether `change`'s file has threads, so its diff is worth placing
    /// them on.
    pub(crate) fn has(&self, change: &FileChange, reverse: bool) -> bool {
        self.roots.contains_key(Self::key(change, reverse).0)
    }

    /// The threads on `change`'s file, in thread order, placed on `diff`,
    /// its line diff. With `reverse` (`-R`) the change and its diff are
    /// reversed, so a thread on the new side shows on the old one.
    ///
    /// A thread on a renamed file's old side shows on the rename, or, when
    /// the diff lists the rename as a deletion and an addition, on the
    /// deletion only: the addition has no old side.
    pub(crate) fn on(&self, change: &FileChange, diff: &FileDiff, reverse: bool) -> Vec<Hung<'_>> {
        let (key, has_old) = Self::key(change, reverse);
        let Some(roots) = self.roots.get(key) else {
            return Vec::new();
        };
        let roots: Vec<&Comment> = (roots.iter())
            .map(|&i| &self.comments[i])
            .filter(|c| {
                let renamed_old = matches!(c.range(), Some((review::Side::Old, _)))
                    && c.old_path.as_ref().is_some_and(|o| *o != c.path);
                has_old || !renamed_old || c.path.as_bytes() != key
            })
            .collect();
        let annotations: Vec<Annotation> = (roots.iter())
            .filter_map(|c| annotation(c))
            .map(|a| Annotation {
                side: match (a.side, reverse) {
                    (side, false) => side,
                    (Side::Old, true) => Side::New,
                    (Side::New, true) => Side::Old,
                },
                ..a
            })
            .collect();
        let placed: HashMap<String, Placement> = dffy_core::annotation::place(&annotations, diff)
            .into_iter()
            .map(|p| (p.id().to_owned(), p))
            .collect();
        roots
            .into_iter()
            .map(|root| {
                let at = match placed.get(&root.id) {
                    None => At::File,
                    Some(Placement::Placed { side, lines, .. }) => At::Line {
                        side: *side,
                        lines: lines.clone(),
                    },
                    Some(Placement::Lost { .. }) => At::Lost,
                };
                let replies = self.replies.get(&root.id).into_iter().flatten();
                Hung {
                    thread: std::iter::once(root)
                        .chain(replies.map(|&i| &self.comments[i]))
                        .collect(),
                    at,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use dffy_core::diff::{Options, hunks};
    use dffy_core::git::Status;

    use super::super::tests::text;
    use super::*;

    /// `change` diffed from `old` to `new`.
    fn diffed(change: FileChange, old: &str, new: &str) -> FileDiff {
        let (old, new) = (text(old), text(new));
        FileDiff {
            change,
            hunks: hunks(&old, &new, &Options::default()),
            old,
            new,
            binary: false,
            too_large: false,
            note: None,
        }
    }

    fn change(path: &str, status: Status, sides: (bool, bool)) -> FileChange {
        let side = |on: bool| {
            on.then_some(dffy_core::git::Side::Worktree {
                mode: dffy_core::git::Mode::File,
                id: None,
            })
        };
        FileChange {
            path: path.into(),
            status,
            old: side(sides.0),
            new: side(sides.1),
            staging: None,
        }
    }

    /// A snapshot of the file places each file's threads on its diff, in
    /// thread order with their replies: on their lines, on the whole file,
    /// or lost; only the branch's threads and those on none.
    #[test]
    fn a_snapshot_places_each_files_threads_on_its_diff() {
        let review = Review::parse(
            br#"{"version":1,"comments":[
{"id":"g","path":"a.rs","side":"new","start":{"old":null,"new":9},"end":{"old":null,"new":9},"text":"Gone","author":"","created":"t","anchor":{"before":[],"lines":["nine"],"after":[]}},
{"id":"n","path":"a.rs","side":"new","start":{"old":null,"new":5},"end":{"old":null,"new":5},"text":"Two","author":"ana","created":"t","anchor":{"before":[],"lines":["two"],"after":[]}},
{"id":"r","path":"a.rs","text":"Yes","author":"","created":"t","in_reply_to":"n"},
{"id":"f","path":"a.rs","text":"File","author":"","created":"t"},
{"id":"o","path":"a.rs","text":"Elsewhere","author":"","created":"t","branch":"other"},
{"id":"b","path":"b.rs","text":"B","author":"","created":"t"}
]}"#,
        )
        .expect("parses");
        let snapshot = Snapshot::new(&review, review::Branches::On(Some("main")));
        let a = change("a.rs", Status::Modified, (true, true));
        let diff = diffed(a.clone(), "one\nthree\n", "one\ntwo\nthree\n");
        let placed: Vec<(Vec<&str>, At)> = (snapshot.on(&a, &diff, false).into_iter())
            .map(|h| (h.thread.iter().map(|c| c.id.as_str()).collect(), h.at))
            .collect();
        let line = |side, lines| At::Line { side, lines };
        assert_eq!(
            placed,
            [
                (vec!["f"], At::File),
                (vec!["n", "r"], line(Side::New, 1..2)),
                (vec!["g"], At::Lost),
            ]
        );
        assert!(snapshot.has(&a, false));
        let c = change("c.rs", Status::Modified, (true, true));
        assert!(!snapshot.has(&c, false));
        assert!(
            snapshot
                .on(&c, &diffed(c.clone(), "x\n", "y\n"), false)
                .is_empty()
        );

        // With -R the change and its diff come reversed: the new side the
        // thread is on is the old side shown.
        let reversed = a.clone().reversed();
        let back = diffed(reversed.clone(), "one\ntwo\nthree\n", "one\nthree\n");
        let at: Vec<At> = (snapshot.on(&reversed, &back, true).into_iter())
            .map(|h| h.at)
            .collect();
        assert_eq!(at, [At::File, line(Side::Old, 1..2), At::Lost]);
    }

    /// A thread on a renamed file's old side shows on the rename when the
    /// diff pairs it, and on the deletion of its old path when it doesn't,
    /// never on both.
    #[test]
    fn an_old_side_thread_shows_once_whether_the_rename_is_paired_or_not() {
        let review = Review::parse(
            br#"{"version":1,"comments":[
{"id":"o","path":"new.rs","old_path":"old.rs","side":"old","start":{"old":2,"new":null},"end":{"old":2,"new":null},"text":"Old","author":"","created":"t","anchor":{"before":[],"lines":["dos"],"after":[]}}
]}"#,
        )
        .expect("parses");
        let snapshot = Snapshot::new(&review, review::Branches::All);
        let ids = |change: &FileChange, old: &str, new: &str| -> Vec<At> {
            let diff = diffed(change.clone(), old, new);
            (snapshot.on(change, &diff, false).into_iter())
                .map(|h| h.at)
                .collect()
        };
        let old_line = At::Line {
            side: Side::Old,
            lines: 1..2,
        };
        let renamed = change(
            "new.rs",
            Status::Renamed {
                from: "old.rs".into(),
                similarity: 90,
            },
            (true, true),
        );
        assert_eq!(
            ids(&renamed, "uno\ndos\n", "uno\ndos\ntres\n"),
            std::slice::from_ref(&old_line)
        );
        let added = change("new.rs", Status::Added, (false, true));
        assert_eq!(ids(&added, "", "uno\ndos\ntres\n"), []);
        let deleted = change("old.rs", Status::Deleted, (true, false));
        assert_eq!(ids(&deleted, "uno\ndos\n", ""), [old_line]);
    }
}

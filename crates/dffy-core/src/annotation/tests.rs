use std::sync::LazyLock;

use super::*;
use crate::diff::{Options, Whitespace, hunks};
use crate::document::{Document, File, Source};
use crate::git::{FileChange, Mode, Side as GitSide, Status};
use crate::highlight::Registry;

static GRAMMARS: LazyLock<Registry> = LazyLock::new(Registry::builtin);

fn text(s: &str) -> Text {
    Text::new(s.as_bytes().to_vec())
}

fn diff(path: &str, old: &str, new: &str) -> FileDiff {
    let side = Some(GitSide::Worktree {
        mode: Mode::File,
        id: None,
    });
    let (old, new) = (text(old), text(new));
    FileDiff {
        change: FileChange {
            path: path.into(),
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
    }
}

fn on(id: &str, side: Side, lines: Range<usize>, anchor: Option<Anchor>) -> Annotation {
    Annotation {
        id: id.to_owned(),
        source: "test",
        path: "a.rs".into(),
        old_path: None,
        side,
        lines,
        anchor,
    }
}

fn placed(id: &str, side: Side, lines: Range<usize>) -> Placement {
    Placement::Placed {
        id: id.to_owned(),
        side,
        lines,
    }
}

fn lost(id: &str) -> Placement {
    Placement::Lost { id: id.to_owned() }
}

#[test]
fn anchors_take_two_lines_of_context_without_terminators() {
    let t = text("one\ntwo\r\nthree\nfour\nfive\nsix");
    assert_eq!(
        Anchor::new(&t, 2..4),
        Anchor {
            before: vec!["one".to_owned(), "two\r".to_owned()],
            lines: vec!["three".to_owned(), "four".to_owned()],
            after: vec!["five".to_owned(), "six".to_owned()],
        }
    );
    let edge = Anchor::new(&t, 0..1);
    assert_eq!(edge.before, [""; 0]);
    assert_eq!(edge.after, ["two\r", "three"]);
    let lossy = Anchor::new(&Text::new(b"a\xff\n".to_vec()), 0..1);
    assert_eq!(lossy.lines, ["a\u{fffd}"]);
    assert_eq!(lossy.place(&Text::new(b"a\xff\n".to_vec()), 0), Some(0..1));
}

/// Exact at the recorded lines wins; else the lines with their context
/// found once nearby; found twice, or not at all, they're lost, as is an
/// anchor without lines (a file written by hand).
#[test]
fn anchors_find_their_lines_exactly_or_once_nearby() {
    let original = text("a\nb\nx\ny\nc\nd\n");
    let anchor = Anchor::new(&original, 2..4);

    assert_eq!(anchor.place(&original, 2), Some(2..4), "exact");
    let moved = text("new\nnew\na\nb\nx\ny\nc\nd\n");
    assert_eq!(anchor.place(&moved, 2), Some(4..6), "moved, found once");
    let twice = text("a\nb\nx\ny\nc\nd\nz\na\nb\nx\ny\nc\nd\n");
    assert_eq!(anchor.place(&twice, 2), Some(2..4), "exact wins");
    assert_eq!(anchor.place(&twice, 3), None, "ambiguous");
    let edited = text("a\nb\nx\nY\nc\nd\n");
    assert_eq!(anchor.place(&edited, 2), None, "missing");
    let mut far = "pad\n".repeat(NEARBY + 10);
    far.push_str("a\nb\nx\ny\nc\nd\n");
    assert_eq!(anchor.place(&text(&far), 2), None, "too far away");
    assert_eq!(anchor.place(&text("a\n"), 5), None, "past the end");
    assert_eq!(
        Anchor::default().place(&original, 2),
        None,
        "no lines to find"
    );
}

/// Each annotation is placed on its own side's text; one without an anchor
/// stays at its lines while the side has them.
#[test]
fn annotations_place_on_their_side_of_the_diff() {
    let old = "a\nb\nx\ny\nc\nd\n";
    let new = "new\nnew\na\nb\nx\ny\nc\nd\ntwice\ntwice\n";
    let fd = diff("a.rs", old, new);
    let anchored = |id, side, lines: Range<usize>| {
        let source = if side == Side::Old { &fd.old } else { &fd.new };
        on(id, side, lines.clone(), Some(Anchor::new(source, lines)))
    };
    let annotations = [
        anchored("exact-old", Side::Old, 2..4),
        on(
            "moved",
            Side::New,
            2..4,
            Some(Anchor::new(&text(old), 2..4)),
        ),
        on(
            "ambiguous",
            Side::New,
            0..1,
            Some(Anchor {
                lines: vec!["twice".to_owned()],
                ..Anchor::default()
            }),
        ),
        on(
            "missing",
            Side::Old,
            1..2,
            Some(Anchor {
                lines: vec!["gone".to_owned()],
                ..Anchor::default()
            }),
        ),
        on("bare", Side::New, 9..10, None),
        on("bare-past-the-end", Side::Old, 5..7, None),
    ];
    assert_eq!(
        place(&annotations, &fd),
        [
            placed("exact-old", Side::Old, 2..4),
            placed("moved", Side::New, 4..6),
            lost("ambiguous"),
            lost("missing"),
            placed("bare", Side::New, 9..10),
            lost("bare-past-the-end"),
        ]
    );
}

/// An old-side annotation on a renamed file sits on its path's diff and on
/// its old path's too, for a list that shows a deletion and an addition;
/// a new-side one only on its path's.
#[test]
fn an_old_side_annotation_of_a_rename_sits_on_both_paths() {
    let renamed = |id: &str, side| Annotation {
        path: "new.rs".into(),
        old_path: Some("old.rs".into()),
        ..on(id, side, 0..1, None)
    };
    let mut set = AnnotationSet::default();
    set.replace([renamed("o", Side::Old), renamed("n", Side::New)]);
    let ids = |path: &str| -> Vec<String> {
        set.on(path.as_bytes())
            .map(|a| a.annotations.iter().map(|a| a.id.clone()).collect())
            .unwrap_or_default()
    };
    assert_eq!(ids("new.rs"), ["o", "n"]);
    assert_eq!(ids("old.rs"), ["o"]);
    let deleted = diff("old.rs", "kept\n", "");
    let annotations = &set.on(b"old.rs").expect("on the old path").annotations;
    assert_eq!(place(annotations, &deleted), [placed("o", Side::Old, 0..1)]);
}

/// Only the paths whose annotations changed take the new generation, so a
/// placement on the others stays current; one made before a path's change
/// isn't.
#[test]
fn a_change_restamps_only_its_paths() {
    let mut set = AnnotationSet::default();
    let b = Annotation {
        path: "b.rs".into(),
        ..on("b", Side::New, 0..1, None)
    };
    assert_eq!(
        set.replace([on("a", Side::New, 0..1, None), b.clone()]),
        ["a.rs", "b.rs"]
    );
    let first = set.generation();
    assert!(set.is_current(b"a.rs", first) && set.is_current(b"b.rs", first));
    assert_eq!(
        set.replace([on("a", Side::New, 3..4, None), b.clone()]),
        ["a.rs"]
    );
    assert!(!set.is_current(b"a.rs", first), "a.rs moved on");
    assert!(set.is_current(b"b.rs", first));
    assert_eq!(
        set.on(b"a.rs").map(|a| a.generation),
        Some(set.generation())
    );
    let second = set.generation();
    assert_eq!(
        set.replace([on("a", Side::New, 3..4, None), b]),
        Vec::<BString>::new()
    );
    assert_eq!(set.generation(), second, "nothing changed");
    assert_eq!(set.replace([]), ["a.rs", "b.rs"], "removed");
    assert!(set.is_current(b"a.rs", 0), "nothing left to place");
}

/// A placement made for one set is never current for another that took
/// its place, though both hold the path.
#[test]
fn another_sets_placement_is_not_current() {
    let (mut old, mut new) = (AnnotationSet::default(), AnnotationSet::default());
    old.replace([on("a", Side::New, 0..1, None)]);
    let placed = old.generation();
    new.replace([on("x", Side::New, 5..6, None)]);
    assert!(!new.is_current(b"a.rs", placed));
}

/// The lines a gap hides around a placed annotation show, with two lines
/// around them; a lost one reveals nothing.
#[test]
fn placed_annotations_reveal_their_lines() {
    let old = (1..=80)
        .map(|i| format!("line {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    let new = old
        .replace("line 3\n", "line three\n")
        .replace("line 60\n", "line sixty\n");
    let fd = diff("a.rs", &old, &new);
    let mut doc = Document::<()>::default();
    doc.push(fd.change.clone(), Source::Repo);
    let loaded = File::prepare(&GRAMMARS, None).file(fd, Whitespace::Exact, false);
    let _ = doc.load(0, Ok(Box::new(loaded)));
    assert_eq!(doc.hidden_lines(0), Some(vec![6..56, 63..80]));
    let unified = |_: &crate::view::HunkHeader<'_>| crate::view::Arrangement::Unified;
    assert!(!doc.reveal_annotations(0, &[lost("gone")], unified));
    assert!(doc.reveal_annotations(0, &[placed("a", Side::New, 29..30)], unified));
    assert_eq!(doc.hidden_lines(0), Some(vec![6..27, 32..56, 63..80]));
}

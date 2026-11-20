//! Applying a review suggestion to the working tree, checked against what
//! the file and the index then hold.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::ops::{ControlFlow, Range};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

use dffy_core::annotation::{Anchor, apply_suggestion, check_suggestion};
use dffy_core::diff::{LineKind, Options};
use dffy_core::git::{Error, FileDiff, NewSide, Repo, Revisions};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 output")
}

fn write(dir: &Path, path: &str, content: &str) {
    std::fs::write(dir.join(path), content).expect("write");
}

fn read(dir: &Path, path: &str) -> Vec<u8> {
    std::fs::read(dir.join(path)).expect("read")
}

/// A repository with `path` committed as `committed` and changed to
/// `worktree`, with `.gitattributes` holding `attributes` when given.
fn fixture(path: &str, committed: &str, worktree: &str, attributes: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    if !attributes.is_empty() {
        write(d, ".gitattributes", attributes);
    }
    write(d, path, committed);
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    write(d, path, worktree);
    dir
}

/// Which diff the viewer shows.
#[derive(Clone, Copy)]
enum View {
    /// Plain `dffy`: `HEAD` against the working tree.
    Plain,
    Staged,
    Unstaged,
}

/// `path`'s diff as `view` shows it.
fn shown(dir: &Path, view: View, path: &str) -> (Repo, FileDiff) {
    let repo = Repo::discover(dir).expect("repo");
    let revisions = Revisions {
        staged: matches!(view, View::Staged),
        unstaged: matches!(view, View::Unstaged),
        ..Revisions::default()
    };
    let spec = repo.resolve_revisions(&revisions).expect("resolves");
    let mut found = None;
    repo.for_each_change(&spec, &[path.into()], |c| {
        found = Some(c);
        ControlFlow::Break(())
    })
    .expect("lists");
    let diff = repo
        .file_diff(found.expect("listed"), &Options::default())
        .expect("diffs");
    (repo, diff)
}

/// A suggestion on `lines` of the new side as `diff` shows it.
fn on(diff: &FileDiff, lines: Range<usize>) -> (usize, Anchor) {
    (lines.start, Anchor::new(&diff.new, lines))
}

/// Refused, and nothing written: the file is as it was and no temporary
/// file is left beside it.
fn refused(dir: &Path, path: &str, result: Result<Range<usize>, Error>, before: &[u8]) -> String {
    let why = match result {
        Err(e @ Error::Refused(_)) => e.to_string(),
        other => panic!("not refused: {other:?}"),
    };
    if before.is_empty() {
        assert!(!dir.join(path).exists(), "{why}");
    } else {
        assert_eq!(read(dir, path), before, "{why}");
    }
    assert_eq!(leftovers(dir), Vec::<String>::new());
    why
}

fn leftovers(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("lists")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("dffy"))
        .collect();
    names.sort();
    names
}

/// The removed and the added lines of `diff`, without terminators.
fn changed(diff: &FileDiff) -> (Vec<String>, Vec<String>) {
    let text = |t: &dffy_core::diff::Text, i| {
        String::from_utf8_lossy(t.line(i))
            .trim_end_matches('\n')
            .to_owned()
    };
    let (mut removed, mut added) = (Vec::new(), Vec::new());
    for line in diff.hunks.iter().flat_map(|h| &h.lines) {
        match line.kind {
            LineKind::Removed => removed.push(text(&diff.old, line.old)),
            LineKind::Added => added.push(text(&diff.new, line.new)),
            LineKind::Context => {}
        }
    }
    (removed, added)
}

const FILE: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";

/// Applied where it was written, on a file plain `dffy` shows staged: the
/// working tree has the replacement in place of the anchored lines, the
/// index is untouched, and a diff of the working tree against the index
/// shows exactly that change.
#[test]
fn a_suggestion_replaces_its_lines_in_the_working_tree_only() {
    let edited = FILE.replace("2\n", "two\n");
    let dir = fixture("f.txt", FILE, &edited, "");
    let d = dir.path();
    git(d, &["add", "f.txt"]);
    let (repo, diff) = shown(d, View::Plain, "f.txt");
    let (at, anchor) = on(&diff, 4..6);
    let placed = apply_suggestion(
        &repo,
        &diff,
        NewSide::AsShown,
        at,
        &anchor,
        &["five", "six", "more"],
    )
    .expect("applies");
    assert_eq!(placed, 4..6);
    assert_eq!(
        String::from_utf8(read(d, "f.txt")).expect("utf-8"),
        "1\ntwo\n3\n4\nfive\nsix\nmore\n7\n8\n9\n10\n"
    );
    assert_eq!(git(d, &["show", ":f.txt"]), edited);
    assert_eq!(leftovers(d), Vec::<String>::new());

    let (_, after) = shown(d, View::Unstaged, "f.txt");
    let lines = |l: &[&str]| l.iter().map(|&s| s.to_owned()).collect::<Vec<_>>();
    assert_eq!(
        changed(&after),
        (lines(&["5", "6"]), lines(&["five", "six", "more"]))
    );
}

/// Lines added above the anchored ones since the diff was shown: the
/// suggestion lands where they are now.
#[test]
fn a_suggestion_follows_its_lines_when_they_moved() {
    let edited = FILE.replace("2\n", "two\n");
    let dir = fixture("f.txt", FILE, &edited, "");
    let d = dir.path();
    let (repo, diff) = shown(d, View::Plain, "f.txt");
    let (at, anchor) = on(&diff, 4..5);
    write(d, "f.txt", &format!("new\nnewer\n{edited}"));
    let placed =
        apply_suggestion(&repo, &diff, NewSide::AsShown, at, &anchor, &["five"]).expect("applies");
    assert_eq!(placed, 6..7);
    assert_eq!(
        String::from_utf8(read(d, "f.txt")).expect("utf-8"),
        format!("new\nnewer\n{}", edited.replace("5\n", "five\n"))
    );
}

/// The anchored lines edited since, or found twice once they moved:
/// refused, and nothing written.
#[test]
fn a_suggestion_whose_lines_changed_or_cannot_be_told_apart_is_refused() {
    let edited = "p\nq\ntarget\nr\ns\nend\n";
    let dir = fixture("f.txt", "start\n", edited, "");
    let d = dir.path();
    let (repo, diff) = shown(d, View::Plain, "f.txt");
    let (at, anchor) = on(&diff, 2..3);

    let changed = edited.replace("target", "TARGET");
    write(d, "f.txt", &changed);
    let why = refused(
        d,
        "f.txt",
        apply_suggestion(&repo, &diff, NewSide::AsShown, at, &anchor, &["x"]),
        changed.as_bytes(),
    );
    assert!(why.contains("changed since"), "{why}");

    let twice = "new\np\nq\ntarget\nr\ns\np\nq\ntarget\nr\ns\n";
    write(d, "f.txt", twice);
    let why = refused(
        d,
        "f.txt",
        apply_suggestion(&repo, &diff, NewSide::AsShown, at, &anchor, &["x"]),
        twice.as_bytes(),
    );
    assert!(why.contains("more than once"), "{why}");
}

/// CRLF lines stay CRLF: in a file committed with them, and in one git
/// checks out with them (`eol=crlf`), where the diff shows LF lines.
#[test]
fn crlf_line_endings_are_kept() {
    let dir = fixture("f.txt", "a\r\nb\r\nc\r\n", "a\r\nB\r\nc\r\n", "");
    let d = dir.path();
    let (repo, diff) = shown(d, View::Plain, "f.txt");
    let (at, anchor) = on(&diff, 1..2);
    apply_suggestion(
        &repo,
        &diff,
        NewSide::AsShown,
        at,
        &anchor,
        &["bee", "buzz"],
    )
    .expect("applies");
    assert_eq!(read(d, "f.txt"), b"a\r\nbee\r\nbuzz\r\nc\r\n");

    let dir = fixture(
        "f.txt",
        "a\r\nb\r\nc\r\n",
        "a\r\nB\r\nc\r\n",
        "f.txt text eol=crlf\n",
    );
    let d = dir.path();
    let (repo, diff) = shown(d, View::Plain, "f.txt");
    assert_eq!(diff.new.bytes(), b"a\nB\nc\n");
    let (at, anchor) = on(&diff, 1..2);
    apply_suggestion(
        &repo,
        &diff,
        NewSide::AsShown,
        at,
        &anchor,
        &["bee", "buzz"],
    )
    .expect("applies");
    assert_eq!(read(d, "f.txt"), b"a\r\nbee\r\nbuzz\r\nc\r\n");
}

/// A file without a newline at the end keeps it that way, whether its last
/// line is replaced or removed.
#[test]
fn a_missing_newline_at_the_end_stays_missing() {
    let dir = fixture("f.txt", "a\nb\nc", "a\nB\nc", "");
    let d = dir.path();
    let (repo, diff) = shown(d, View::Plain, "f.txt");
    let (at, anchor) = on(&diff, 1..3);
    apply_suggestion(&repo, &diff, NewSide::AsShown, at, &anchor, &["b", "see"]).expect("applies");
    assert_eq!(read(d, "f.txt"), b"a\nb\nsee");

    let (repo, diff) = shown(d, View::Plain, "f.txt");
    let (at, anchor) = on(&diff, 1..3);
    apply_suggestion(&repo, &diff, NewSide::AsShown, at, &anchor, &[]).expect("applies");
    assert_eq!(read(d, "f.txt"), b"a");
}

#[test]
fn the_file_keeps_its_permissions() {
    let dir = fixture("f.txt", FILE, &FILE.replace("2\n", "two\n"), "");
    let d = dir.path();
    let permissions = std::fs::Permissions::from_mode(0o640);
    std::fs::set_permissions(d.join("f.txt"), permissions).expect("chmod");
    let (repo, diff) = shown(d, View::Plain, "f.txt");
    let (at, anchor) = on(&diff, 0..1);
    apply_suggestion(&repo, &diff, NewSide::AsShown, at, &anchor, &["one"]).expect("applies");
    let mode = std::fs::metadata(d.join("f.txt"))
        .expect("stat")
        .permissions();
    assert_eq!(mode.mode() & 0o7777, 0o640);
}

/// Only the working tree is written: a diff whose new side is the index
/// or a commit is refused before anything is read.
#[test]
fn a_suggestion_on_a_side_that_isnt_the_working_tree_is_refused() {
    let edited = FILE.replace("2\n", "two\n");
    let dir = fixture("f.txt", FILE, &edited, "");
    let d = dir.path();
    git(d, &["add", "f.txt"]);
    let (repo, diff) = shown(d, View::Staged, "f.txt");
    assert!(
        check_suggestion(&diff, NewSide::AsShown).is_err(),
        "before any I/O"
    );
    let (at, anchor) = on(&diff, 1..2);
    let why = refused(
        d,
        "f.txt",
        apply_suggestion(&repo, &diff, NewSide::AsShown, at, &anchor, &["2"]),
        edited.as_bytes(),
    );
    assert!(why.contains("working tree"), "{why}");
}

/// A diff between commits, as a review's, applies when the caller says
/// its new side is HEAD's tree, the commit the working tree came from: the
/// lines are found in the file on disk all the same, and refused when
/// they changed there. Without that, it's refused before any I/O.
#[test]
fn a_suggestion_on_heads_tree_applies_when_the_caller_vouches_for_it() {
    let edited = FILE.replace("2\n", "two\n");
    let dir = fixture("f.txt", FILE, &edited, "");
    let d = dir.path();
    git(d, &["commit", "-q", "-am", "head"]);
    let repo = Repo::discover(d).expect("repo");
    let revisions = Revisions {
        revs: vec!["HEAD~1..HEAD".to_owned()],
        ..Revisions::default()
    };
    let spec = repo.resolve_revisions(&revisions).expect("resolves");
    let mut found = None;
    repo.for_each_change(&spec, &["f.txt".into()], |c| {
        found = Some(c);
        ControlFlow::Break(())
    })
    .expect("lists");
    let diff = repo
        .file_diff(found.expect("listed"), &Options::default())
        .expect("diffs");
    let (at, anchor) = on(&diff, 1..2);

    assert!(check_suggestion(&diff, NewSide::AsShown).is_err());
    let why = refused(
        d,
        "f.txt",
        apply_suggestion(&repo, &diff, NewSide::AsShown, at, &anchor, &["2"]),
        edited.as_bytes(),
    );
    assert!(why.contains("working tree"), "{why}");

    let changed = edited.replace("two", "TWO");
    write(d, "f.txt", &changed);
    let why = refused(
        d,
        "f.txt",
        apply_suggestion(&repo, &diff, NewSide::Head, at, &anchor, &["2"]),
        changed.as_bytes(),
    );
    assert!(why.contains("changed since"), "{why}");

    write(d, "f.txt", &edited);
    apply_suggestion(&repo, &diff, NewSide::Head, at, &anchor, &["2"]).expect("applies");
    assert_eq!(read(d, "f.txt"), FILE.as_bytes());
}

/// Binary files, files gone from the working tree and files git converts
/// other than by line endings (here `ident`) are refused.
#[test]
fn binary_missing_and_filtered_files_are_refused() {
    let dir = fixture("bin", "a\0b", "a\0c", "");
    let d = dir.path();
    let (repo, diff) = shown(d, View::Plain, "bin");
    let anchor = Anchor {
        lines: vec!["a".to_owned()],
        ..Anchor::default()
    };
    let why = refused(
        d,
        "bin",
        apply_suggestion(&repo, &diff, NewSide::AsShown, 0, &anchor, &["x"]),
        b"a\0c",
    );
    assert!(why.contains("binary"), "{why}");

    // Text when shown, binary by the time it's applied.
    let dir = fixture("f.txt", "a\n", "b\n", "");
    let d = dir.path();
    let (repo, diff) = shown(d, View::Plain, "f.txt");
    let (at, anchor) = on(&diff, 0..1);
    std::fs::write(d.join("f.txt"), b"b\n\0").expect("write");
    let why = refused(
        d,
        "f.txt",
        apply_suggestion(&repo, &diff, NewSide::AsShown, at, &anchor, &["x"]),
        b"b\n\0",
    );
    assert!(why.contains("binary"), "{why}");

    std::fs::remove_file(d.join("f.txt")).expect("rm");
    let why = refused(
        d,
        "f.txt",
        apply_suggestion(&repo, &diff, NewSide::AsShown, at, &anchor, &["x"]),
        b"",
    );
    assert!(why.contains("isn't in the working tree"), "{why}");

    let dir = fixture(
        "f.txt",
        "$Id$\nline\n",
        "$Id: 0123 $\nLINE\n",
        "f.txt ident\n",
    );
    let d = dir.path();
    let (repo, diff) = shown(d, View::Plain, "f.txt");
    let (at, anchor) = on(&diff, 1..2);
    let why = refused(
        d,
        "f.txt",
        apply_suggestion(&repo, &diff, NewSide::AsShown, at, &anchor, &["x"]),
        b"$Id: 0123 $\nLINE\n",
    );
    assert!(why.contains("filter"), "{why}");
}

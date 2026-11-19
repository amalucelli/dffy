//! Staging, unstaging and discarding a hunk or picked lines of an
//! uncommitted diff, checked against what git then has in the index and the
//! working tree.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::ops::ControlFlow;
use std::path::Path;
use std::process::Command;

use dffy_core::diff::Options;
use dffy_core::git::{Edit, Error, FileDiff, Picked, Repo, Revisions};

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

/// A repository with `path` committed as `committed` and changed to
/// `worktree`.
fn repo(path: &str, committed: &str, worktree: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
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

/// Applies `edit` to `picked` lines of `path` as `view` shows it, and
/// returns how many lines it took.
fn apply(
    dir: &Path,
    view: View,
    path: &str,
    edit: Edit,
    picked: impl Fn(&FileDiff) -> Picked,
) -> usize {
    let (repo, diff) = shown(dir, view, path);
    repo.apply_lines(&diff, &picked(&diff), edit)
        .unwrap_or_else(|e| panic!("{e}"))
        .lines
}

fn refused(
    dir: &Path,
    view: View,
    path: &str,
    edit: Edit,
    picked: impl Fn(&FileDiff) -> Picked,
) -> String {
    let (repo, diff) = shown(dir, view, path);
    match repo.apply_lines(&diff, &picked(&diff), edit) {
        Err(e @ Error::Refused(_)) => e.to_string(),
        other => panic!("not refused: {other:?}"),
    }
}

fn hunk(i: usize) -> impl Fn(&FileDiff) -> Picked {
    move |diff| Picked::hunk(&diff.hunks[i])
}

fn all(diff: &FileDiff) -> Picked {
    let mut picked = Picked::default();
    for h in &diff.hunks {
        let p = Picked::hunk(h);
        picked.old.extend(p.old);
        picked.new.extend(p.new);
    }
    picked
}

/// Lines by 0-based index: of the old side, then of the new.
fn lines(old: &'static [usize], new: &'static [usize]) -> impl Fn(&FileDiff) -> Picked {
    move |_| Picked {
        old: old.to_vec(),
        new: new.to_vec(),
    }
}

fn index(dir: &Path, path: &str) -> String {
    git(dir, &["show", &format!(":{path}")])
}

fn worktree(dir: &Path, path: &str) -> String {
    std::fs::read_to_string(dir.join(path)).expect("read")
}

const TEN: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n";

fn ten_with(edits: &[(&str, &str)]) -> String {
    edits.iter().fold(TEN.to_owned(), |text, (from, to)| {
        text.replacen(from, to, 1)
    })
}

#[test]
fn staging_a_hunk_takes_only_that_hunk() {
    let edited = ten_with(&[("2\n", "two\n"), ("18\n", "eighteen\n")]);
    let dir = repo("f.txt", TEN, &edited);
    let d = dir.path();
    assert_eq!(apply(d, View::Unstaged, "f.txt", Edit::Stage, hunk(1)), 2);
    assert_eq!(index(d, "f.txt"), ten_with(&[("18\n", "eighteen\n")]));
    assert_eq!(worktree(d, "f.txt"), edited);
}

/// As `git add -p` edits a hunk: a removed line left out stays, an added
/// line left out isn't added.
#[test]
fn staging_picked_lines_keeps_removals_left_out_and_drops_additions() {
    let edited = ten_with(&[("5\n", "five\nfive more\n")]);
    // Old line 4 is "5"; new lines 4 and 5 are "five" and "five more".
    let cases: [(&[usize], &[usize], usize, &str); 4] = [
        (&[], &[4], 1, "4\n5\nfive\n6\n"),
        (&[4], &[], 1, "4\n6\n"),
        (&[4], &[5], 2, "4\nfive more\n6\n"),
        (&[4], &[4, 5], 3, "4\nfive\nfive more\n6\n"),
    ];
    for (old, new, taken, around) in cases {
        let dir = repo("f.txt", TEN, &edited);
        let d = dir.path();
        assert_eq!(
            apply(d, View::Unstaged, "f.txt", Edit::Stage, move |_| Picked {
                old: old.to_vec(),
                new: new.to_vec(),
            }),
            taken
        );
        let staged = index(d, "f.txt");
        assert!(
            staged.contains(&format!("3\n{around}7\n")),
            "{old:?} {new:?}: {staged:?}"
        );
    }
}

#[test]
fn hunks_at_the_start_and_end_of_a_file() {
    let edited = format!("zero\n{}21\n", TEN.replacen("1\n", "one\n", 1));
    let dir = repo("f.txt", TEN, &edited);
    let d = dir.path();
    // Only the first line's new version: the old one stays above it.
    apply(d, View::Unstaged, "f.txt", Edit::Stage, lines(&[], &[1]));
    assert_eq!(index(d, "f.txt"), format!("1\none\n{}", &TEN[2..]));
    apply(d, View::Unstaged, "f.txt", Edit::Stage, |diff| {
        Picked::hunk(diff.hunks.last().expect("a hunk"))
    });
    assert!(index(d, "f.txt").ends_with("20\n21\n"));
    apply(d, View::Unstaged, "f.txt", Edit::Stage, all);
    assert_eq!(index(d, "f.txt"), edited);
}

#[test]
fn a_missing_newline_at_the_end_goes_with_its_line() {
    let dir = repo("f.txt", "a\nb", "a\nb\nc");
    let d = dir.path();
    let why = refused(d, View::Unstaged, "f.txt", Edit::Stage, lines(&[], &[2]));
    assert!(why.contains("newline"), "{why}");
    assert_eq!(index(d, "f.txt"), "a\nb");
    apply(d, View::Unstaged, "f.txt", Edit::Stage, all);
    assert_eq!(index(d, "f.txt"), "a\nb\nc");
    // Taking the line back out restores the missing newline.
    apply(d, View::Staged, "f.txt", Edit::Unstage, all);
    assert_eq!(index(d, "f.txt"), "a\nb");
}

/// Discarding restores the working tree from the index, so it takes only
/// what a diff of the working tree shows: lines shown staged are refused,
/// and the working tree keeps its edit.
#[test]
fn staged_lines_are_not_discarded() {
    let edited = ten_with(&[("2\n", "two\n")]);
    let dir = repo("f.txt", TEN, &ten_with(&[("2\n", "deux\n")]));
    let d = dir.path();
    write(d, "f.txt", &edited);
    git(d, &["add", "f.txt"]);
    write(
        d,
        "f.txt",
        &ten_with(&[("2\n", "two\n"), ("9\n", "nine\n")]),
    );
    let why = refused(d, View::Staged, "f.txt", Edit::Discard, all);
    assert!(why.contains("unstage"), "{why}");
    let (_, diff) = shown(d, View::Staged, "f.txt");
    assert!(Edit::Discard.check(&diff).is_err(), "before any I/O");
    assert!(worktree(d, "f.txt").contains("nine"));
}

#[test]
fn crlf_lines_are_staged_as_they_are() {
    let dir = repo("f.txt", "a\r\nb\r\nc\r\n", "a\r\nB\r\nc\r\nd\r\n");
    let d = dir.path();
    apply(d, View::Unstaged, "f.txt", Edit::Stage, lines(&[1], &[1]));
    assert_eq!(index(d, "f.txt"), "a\r\nB\r\nc\r\n");
}

#[test]
fn unstaging_reverses_staging_line_by_line() {
    let edited = ten_with(&[("5\n", "five\nfive more\n"), ("18\n", "eighteen\n")]);
    let dir = repo("f.txt", TEN, &edited);
    let d = dir.path();
    git(d, &["add", "f.txt"]);
    // In reverse, an added line left out stays, a removed one isn't put back.
    assert_eq!(
        apply(d, View::Staged, "f.txt", Edit::Unstage, lines(&[4], &[4])),
        2
    );
    let staged = index(d, "f.txt");
    assert!(staged.contains("4\n5\nfive more\n6\n"), "{staged:?}");
    assert!(staged.contains("eighteen"), "{staged:?}");
    apply(d, View::Staged, "f.txt", Edit::Unstage, all);
    assert_eq!(index(d, "f.txt"), TEN);
    assert_eq!(worktree(d, "f.txt"), edited);
}

#[test]
fn discarding_restores_the_working_tree_from_the_index() {
    let edited = ten_with(&[("5\n", "five\nfive more\n"), ("18\n", "eighteen\n")]);
    let dir = repo("f.txt", TEN, &edited);
    let d = dir.path();
    apply(d, View::Unstaged, "f.txt", Edit::Discard, lines(&[], &[5]));
    let now = worktree(d, "f.txt");
    assert!(now.contains("4\nfive\n6\n"), "{now:?}");
    apply(d, View::Unstaged, "f.txt", Edit::Discard, hunk(0));
    assert_eq!(worktree(d, "f.txt"), ten_with(&[("18\n", "eighteen\n")]));
    assert_eq!(index(d, "f.txt"), TEN);
}

/// Lines edited after the diff was shown aren't discarded: the discard is
/// refused, and the edit stays. An edit elsewhere in the file doesn't stand
/// in the way, and stays too.
#[test]
fn discarding_lines_edited_since_they_were_shown_is_refused() {
    let edited = ten_with(&[("5\n", "five\n")]);
    let dir = repo("f.txt", TEN, &edited);
    let d = dir.path();
    let (repo, diff) = shown(d, View::Unstaged, "f.txt");
    let again = ten_with(&[("5\n", "FIVE\n")]);
    write(d, "f.txt", &again);
    let why = match repo.apply_lines(&diff, &Picked::hunk(&diff.hunks[0]), Edit::Discard) {
        Err(e @ Error::Refused(_)) => e.to_string(),
        other => panic!("not refused: {other:?}"),
    };
    assert!(why.contains("changed since it was shown"), "{why}");
    assert_eq!(worktree(d, "f.txt"), again);

    let (repo, diff) = shown(d, View::Unstaged, "f.txt");
    write(
        d,
        "f.txt",
        &ten_with(&[("5\n", "FIVE\n"), ("18\n", "eighteen\n")]),
    );
    repo.apply_lines(&diff, &Picked::hunk(&diff.hunks[0]), Edit::Discard)
        .expect("discards");
    assert_eq!(worktree(d, "f.txt"), ten_with(&[("18\n", "eighteen\n")]));
}

/// The discard's plan counts the lines it would take back, those of the
/// working tree only, and where they are in it; nothing is written.
#[test]
fn a_discard_plan_counts_only_unstaged_lines() {
    let edited = ten_with(&[("2\n", "two\n"), ("5\n", "five\nfive more\n")]);
    let dir = repo("f.txt", TEN, &edited);
    let d = dir.path();
    git(d, &["add", "f.txt"]);
    write(
        d,
        "f.txt",
        &ten_with(&[("2\n", "two\n"), ("5\n", "FIVE\n")]),
    );
    let (repo, diff) = shown(d, View::Plain, "f.txt");
    // The plain diff shows 4 changed lines; only those of line 5 differ
    // from the index, where it's 2 lines: 3 in all, at line 5.
    assert_eq!(all(&diff).changed(&diff), 4);
    let planned = repo
        .plan_lines(&diff, &all(&diff), Edit::Discard)
        .expect("plans");
    assert_eq!((planned.lines, planned.at), (3, Some((5, 5))));
    assert_eq!(
        worktree(d, "f.txt"),
        ten_with(&[("2\n", "two\n"), ("5\n", "FIVE\n")])
    );
}

/// Unstaging picked lines of two hunks, the first of which adds a line:
/// the second hunk's lines are found where the first one moved them.
#[test]
fn unstaging_lines_of_two_hunks_after_one_that_adds_a_line() {
    let edited = ten_with(&[("5\n", "five\nfive more\n"), ("18\n", "eighteen\n")]);
    let dir = repo("f.txt", TEN, &edited);
    let d = dir.path();
    git(d, &["add", "f.txt"]);
    // New lines 5 ("five more") and 18 ("eighteen"), old line 17 ("18").
    assert_eq!(
        apply(
            d,
            View::Staged,
            "f.txt",
            Edit::Unstage,
            lines(&[17], &[5, 18])
        ),
        3
    );
    assert_eq!(index(d, "f.txt"), ten_with(&[("5\n", "five\n")]));
    assert_eq!(worktree(d, "f.txt"), edited);
}

/// With `eol=crlf` the index holds LF lines and the working tree CRLF ones:
/// staging and discarding read the working tree as git stores it, and the
/// file keeps its CRLF lines.
#[test]
fn staging_and_discarding_with_crlf_checkout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    write(d, ".gitattributes", "f.txt text eol=crlf\n");
    write(d, "f.txt", "a\r\nb\r\nc\r\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    assert_eq!(index(d, "f.txt"), "a\nb\nc\n");
    write(d, "f.txt", "a\r\nB\r\nc\r\nd\r\n");
    apply(d, View::Unstaged, "f.txt", Edit::Stage, lines(&[1], &[1]));
    assert_eq!(index(d, "f.txt"), "a\nB\nc\n");
    apply(d, View::Unstaged, "f.txt", Edit::Discard, all);
    assert_eq!(worktree(d, "f.txt"), "a\r\nB\r\nc\r\n");
}

/// Plain `dffy` shows `HEAD` against the working tree, so after one hunk is
/// staged the file is partly staged and every hunk is still on screen.
#[test]
fn partly_staged_files_take_each_hunk_in_turn_from_the_plain_diff() {
    let edited = ten_with(&[("2\n", "two\n"), ("10\n", "ten\n"), ("18\n", "eighteen\n")]);
    let dir = repo("f.txt", TEN, &edited);
    let d = dir.path();
    apply(d, View::Plain, "f.txt", Edit::Stage, hunk(0));
    apply(d, View::Plain, "f.txt", Edit::Stage, hunk(1));
    assert_eq!(
        index(d, "f.txt"),
        ten_with(&[("2\n", "two\n"), ("10\n", "ten\n")])
    );
    // Already staged: nothing left to stage there.
    let why = refused(d, View::Plain, "f.txt", Edit::Stage, hunk(0));
    assert!(why.contains("nothing to stage"), "{why}");
    apply(d, View::Plain, "f.txt", Edit::Unstage, hunk(0));
    assert_eq!(index(d, "f.txt"), ten_with(&[("10\n", "ten\n")]));
    apply(d, View::Plain, "f.txt", Edit::Discard, hunk(2));
    assert_eq!(
        worktree(d, "f.txt"),
        ten_with(&[("2\n", "two\n"), ("10\n", "ten\n")])
    );
}

/// A line staged, then edited again: in the plain diff its `HEAD` version
/// and its working-tree version stand for the index's version between.
#[test]
fn a_line_staged_then_edited_again_moves_as_one_change() {
    let setup = || {
        let dir = repo("f.txt", "1\n2\n3\n", "1\ntwo\n3\n");
        git(dir.path(), &["add", "f.txt"]);
        write(dir.path(), "f.txt", "1\nTWO\n3\n");
        dir
    };
    let dir = setup();
    apply(dir.path(), View::Plain, "f.txt", Edit::Stage, all);
    assert_eq!(index(dir.path(), "f.txt"), "1\nTWO\n3\n");
    let dir = setup();
    apply(dir.path(), View::Plain, "f.txt", Edit::Unstage, all);
    assert_eq!(index(dir.path(), "f.txt"), "1\n2\n3\n");
    let dir = setup();
    apply(dir.path(), View::Plain, "f.txt", Edit::Discard, all);
    assert_eq!(worktree(dir.path(), "f.txt"), "1\ntwo\n3\n");
}

#[test]
fn untracked_files_stage_picked_lines_and_discard_whole() {
    let dir = repo("f.txt", "a\n", "a\n");
    let d = dir.path();
    write(d, "new.txt", "x\ny\nz\n");
    assert_eq!(
        apply(d, View::Plain, "new.txt", Edit::Stage, lines(&[], &[0, 2])),
        2
    );
    assert_eq!(index(d, "new.txt"), "x\nz\n");
    // Some lines of a new file out of the index leave it there, shorter.
    apply(d, View::Staged, "new.txt", Edit::Unstage, lines(&[], &[0]));
    assert_eq!(index(d, "new.txt"), "z\n");
    git(d, &["rm", "-qf", "--cached", "new.txt"]);
    let (repo, diff) = shown(d, View::Plain, "new.txt");
    let applied = repo
        .apply_lines(&diff, &Picked::hunk(&diff.hunks[0]), Edit::Discard)
        .expect("discards");
    assert!(applied.deleted);
    assert!(!d.join("new.txt").exists());
}

#[test]
fn a_deleted_file_stages_and_discards_whole() {
    let dir = repo("f.txt", "a\nb\n", "a\nb\n");
    let d = dir.path();
    std::fs::remove_file(d.join("f.txt")).expect("rm");
    apply(d, View::Unstaged, "f.txt", Edit::Discard, all);
    assert_eq!(worktree(d, "f.txt"), "a\nb\n");
    std::fs::remove_file(d.join("f.txt")).expect("rm");
    apply(d, View::Unstaged, "f.txt", Edit::Stage, all);
    assert_eq!(git(d, &["ls-files", "f.txt"]), "");
    apply(d, View::Staged, "f.txt", Edit::Unstage, all);
    assert_eq!(index(d, "f.txt"), "a\nb\n");
}

#[test]
fn paths_with_spaces_and_quotes() {
    let name = "a \"b\" c.txt";
    let dir = repo(name, "1\n", "2\n");
    let d = dir.path();
    apply(d, View::Unstaged, name, Edit::Stage, all);
    assert_eq!(index(d, name), "2\n");
}

#[test]
fn binary_files_mode_changes_renames_and_conflicts_are_refused() {
    let dir = repo("bin", "a\0b", "a\0c");
    let why = refused(
        dir.path(),
        View::Unstaged,
        "bin",
        Edit::Stage,
        lines(&[], &[]),
    );
    assert!(why.contains("binary"), "{why}");

    let dir = repo("run.sh", "a\n", "b\n");
    let d = dir.path();
    git(d, &["update-index", "--chmod=+x", "run.sh"]);
    git(d, &["commit", "-q", "-m", "x"]);
    git(d, &["update-index", "--chmod=-x", "run.sh"]);
    let why = refused(d, View::Staged, "run.sh", Edit::Unstage, all);
    assert!(why.contains("mode"), "{why}");

    let dir = repo("old.txt", "1\n2\n3\n", "1\n2\n3\n");
    let d = dir.path();
    git(d, &["mv", "old.txt", "new.txt"]);
    let r = Repo::discover(d).expect("repo");
    let spec = r
        .resolve_revisions(&Revisions {
            staged: true,
            ..Revisions::default()
        })
        .expect("resolves");
    let mut changes = Vec::new();
    r.for_each_change(&spec, &[], |c| {
        changes.push(c);
        ControlFlow::Continue(())
    })
    .expect("lists");
    let renamed = r.detect_renames(changes, None).expect("renames");
    let diff = r
        .file_diff(renamed[0].clone(), &Options::default())
        .expect("diffs");
    let why = r
        .apply_lines(&diff, &Picked::default(), Edit::Unstage)
        .expect_err("refused")
        .to_string();
    assert!(why.contains("renamed"), "{why}");

    let dir = repo("f.txt", "a\n", "a\n");
    let d = dir.path();
    git(d, &["checkout", "-q", "-b", "other"]);
    write(d, "f.txt", "theirs\n");
    git(d, &["commit", "-q", "-am", "theirs"]);
    git(d, &["checkout", "-q", "main"]);
    write(d, "f.txt", "ours\n");
    git(d, &["commit", "-q", "-am", "ours"]);
    let merge = Command::new("git")
        .arg("-C")
        .arg(d)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "merge",
            "-q",
            "other",
        ])
        .output()
        .expect("git runs");
    assert!(!merge.status.success(), "the merge conflicts");
    let why = refused(d, View::Plain, "f.txt", Edit::Stage, all);
    assert!(why.contains("conflict"), "{why}");
}

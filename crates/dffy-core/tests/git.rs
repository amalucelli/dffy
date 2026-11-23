//! Change enumeration and revision resolution checked against the git CLI on
//! small fixture repositories.
#![expect(
    clippy::expect_used,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::Command;

use dffy_core::diff::Options;
use dffy_core::git::{
    ChangeSet, DiffSpec, Error, FileChange, Mode, ObjectId, Repo, Revisions, Side, Snapshot,
    Status, WhichCheckpoint, worktree_file,
};
use dffy_core::view::Staging;

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
    let full = dir.join(path);
    std::fs::create_dir_all(full.parent().expect("has parent")).expect("mkdir");
    std::fs::write(full, content).expect("write");
}

fn init() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    dir
}

fn collect(repo: &Repo, spec: &DiffSpec, paths: &[&str]) -> Vec<FileChange> {
    let paths: Vec<_> = paths.iter().map(|p| (*p).into()).collect();
    let mut out = Vec::new();
    repo.for_each_change(spec, &paths, |c| {
        out.push(c);
        ControlFlow::Continue(())
    })
    .expect("enumerates");
    out
}

fn letter(status: &Status) -> &'static str {
    match status {
        Status::Added => "A",
        Status::Deleted => "D",
        Status::Modified => "M",
        Status::TypeChange => "T",
        Status::Untracked => "??",
        Status::Unmerged => "U",
        Status::Renamed { .. } => "R",
    }
}

/// `(status, path)` pairs, keeping our emission order.
fn ours(changes: &[FileChange]) -> Vec<(String, String)> {
    changes
        .iter()
        .map(|c| {
            let mut path = c.path.to_string();
            if c.status == Status::Untracked && c.new.is_some_and(|s| s.mode() == Mode::Gitlink) {
                path.push('/');
            }
            (letter(&c.status).to_owned(), path)
        })
        .collect()
}

/// Parses `git diff --name-status -z`.
fn name_status(out: &str) -> Vec<(String, String)> {
    let fields: Vec<&str> = out.split('\0').filter(|s| !s.is_empty()).collect();
    fields
        .chunks(2)
        .map(|c| (c[0].to_owned(), c[1].to_owned()))
        .collect()
}

/// The worktree column and untracked entries of `git status --porcelain -z`.
fn status_worktree(out: &str) -> Vec<(String, String)> {
    out.split('\0')
        .filter(|s| s.len() > 3)
        .filter_map(|s| {
            let (code, path) = (&s[..2], &s[3..]);
            match code {
                "??" => Some(("??".to_owned(), path.to_owned())),
                _ if &code[1..] != " " => Some((code[1..].to_owned(), path.to_owned())),
                _ => None,
            }
        })
        .collect()
}

fn diff_name_status(dir: &Path, args: &[&str]) -> Vec<(String, String)> {
    let mut full = vec![
        "diff",
        "--no-ext-diff",
        "--name-status",
        "--no-renames",
        "-z",
    ];
    full.extend_from_slice(args);
    name_status(&git(dir, &full))
}

/// A repository exercising every status the scan distinguishes.
fn messy_repo() -> tempfile::TempDir {
    let dir = init();
    let d = dir.path();
    write(d, "keep.txt", "unchanged\n");
    write(d, "modified.txt", "old\n");
    write(d, "same-size.txt", "aaaa\n");
    write(d, "deleted.txt", "bye\n");
    write(d, "exec.sh", "#!/bin/sh\n");
    write(d, "to-link.txt", "target\n");
    write(d, "touched.txt", "same\n");
    write(d, "staged.txt", "v1\n");
    write(d, "staged-reverted.txt", "v1\n");
    write(d, "sub/dir/deep.c", "int x;\n");
    write(d, "sub/gone.c", "int z;\n");
    write(d, "other/file.txt", "other\n");
    write(d, ".gitignore", "*.log\nbuild/\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);

    write(d, "modified.txt", "new content\n");
    write(d, "same-size.txt", "bbbb\n");
    std::fs::remove_file(d.join("deleted.txt")).expect("rm");
    git(d, &["update-index", "--chmod=+x", "exec.sh"]);
    git(d, &["update-index", "--chmod=-x", "exec.sh"]);
    std::fs::set_permissions(
        d.join("exec.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("chmod");
    std::fs::remove_file(d.join("to-link.txt")).expect("rm");
    std::os::unix::fs::symlink("keep.txt", d.join("to-link.txt")).expect("symlink");
    // Rewritten with the same bytes: stat-dirty, content-clean.
    write(d, "touched.txt", "same\n");
    write(d, "staged.txt", "v2\n");
    git(d, &["add", "staged.txt"]);
    write(d, "staged-reverted.txt", "v2\n");
    git(d, &["add", "staged-reverted.txt"]);
    write(d, "staged-reverted.txt", "v1\n");
    write(d, "sub/dir/deep.c", "int y;\n");
    write(d, "sub/added.c", "int a;\n");
    git(d, &["add", "sub/added.c"]);
    git(d, &["rm", "-q", "--cached", "sub/gone.c"]);
    write(d, "sub/untracked.c", "int u;\n");
    write(d, "other/file.txt", "changed\n");
    write(d, "new-untracked.txt", "hi\n");
    write(d, "newdir/a.txt", "a\n");
    write(d, "newdir/b/c.txt", "c\n");
    write(d, "debug.log", "ignored\n");
    write(d, "build/out.o", "ignored\n");
    write(d, "excluded.tmp", "ignored via info/exclude\n");
    std::fs::write(d.join(".git/info/exclude"), "*.tmp\n").expect("exclude");
    write(d, "intent.txt", "planned\n");
    git(d, &["add", "-N", "intent.txt"]);
    let nested = d.join("nested");
    std::fs::create_dir(&nested).expect("mkdir");
    git(&nested, &["init", "-q"]);
    write(&nested, "inner.txt", "x\n");
    dir
}

#[test]
fn worktree_matches_git_status() {
    let dir = messy_repo();
    let repo = Repo::discover(dir.path()).expect("repo");
    let changes = collect(&repo, &DiffSpec::IndexToWorktree { untracked: true }, &[]);
    let expected = status_worktree(&git(
        dir.path(),
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "-uall",
        ],
    ));
    let sorted = |v: Vec<(String, String)>| v.into_iter().collect::<BTreeSet<_>>();
    assert_eq!(sorted(ours(&changes)), sorted(expected));

    let tracked: Vec<_> = changes
        .iter()
        .filter(|c| c.status != Status::Untracked)
        .collect();
    assert!(
        tracked.windows(2).all(|w| w[0].path < w[1].path),
        "index order"
    );
    let untracked: Vec<_> = changes.iter().skip(tracked.len()).collect();
    assert!(
        untracked.iter().all(|c| c.status == Status::Untracked),
        "untracked come last"
    );
}

#[test]
fn staged_matches_git_diff_cached() {
    let dir = messy_repo();
    let repo = Repo::discover(dir.path()).expect("repo");
    let spec = repo.resolve(&[], true, false).expect("spec");
    assert_eq!(
        ours(&collect(&repo, &spec, &[])),
        diff_name_status(dir.path(), &["--cached"])
    );
}

#[test]
fn tree_to_worktree_matches_git_diff_head() {
    let dir = messy_repo();
    let repo = Repo::discover(dir.path()).expect("repo");
    let spec = repo.resolve(&["HEAD"], false, false).expect("spec");
    assert!(matches!(spec, DiffSpec::TreeToWorktree { .. }));
    let changes = collect(&repo, &spec, &[]);
    assert_eq!(ours(&changes), diff_name_status(dir.path(), &["HEAD"]));
    assert!(
        !changes.iter().any(|c| c.path == "staged-reverted.txt"),
        "a staged change undone in the worktree is no change against HEAD"
    );
}

/// Each changed path's staging as `git status` columns say it: index only
/// is staged, worktree only (or untracked) unstaged, both partly staged.
/// For a path listed twice (a staged deletion left behind as an untracked
/// file), the tracked line wins.
fn git_staging(dir: &Path) -> BTreeMap<String, Staging> {
    let out = git(
        dir,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "-uall",
            "--no-renames",
        ],
    );
    let mut staging = BTreeMap::new();
    for entry in out.split('\0').filter(|s| s.len() > 3) {
        let (x, y, path) = (entry.as_bytes()[0], entry.as_bytes()[1], &entry[3..]);
        let state = match (x, y) {
            (b'?', b'?') | (b' ', _) => Staging::Unstaged,
            (_, b' ') => Staging::Staged,
            _ => Staging::Partly,
        };
        let path = path.trim_end_matches('/').to_owned();
        if x != b'?' || !staging.contains_key(&path) {
            staging.insert(path, state);
        }
    }
    staging
}

/// `(status, path, staging)` rows of a default (everything uncommitted) diff.
fn uncommitted(repo: &Repo, paths: &[&str]) -> Vec<(String, String, Option<Staging>)> {
    let spec = repo.resolve(&[], false, false).expect("default spec");
    assert!(matches!(spec, DiffSpec::Uncommitted { .. }), "{spec:?}");
    let changes = collect(repo, &spec, paths);
    ours(&changes)
        .into_iter()
        .zip(&changes)
        .map(|((status, path), c)| (status, path, c.staging))
        .collect()
}

/// The default diff lists `git diff HEAD` and then the untracked files
/// `git status` shows, each path once, with the staging `git status` gives.
#[test]
fn default_is_git_diff_head_plus_untracked_with_staging() {
    let dir = messy_repo();
    let d = dir.path();
    let repo = Repo::discover(d).expect("repo");
    let rows = uncommitted(&repo, &[]);

    let mut expected = diff_name_status(d, &["HEAD"]);
    let untracked = status_worktree(&git(
        d,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "-uall",
        ],
    ))
    .into_iter()
    .filter(|(code, path)| code == "??" && !expected.iter().any(|(_, p)| p == path))
    .collect::<Vec<_>>();
    expected.extend(untracked);
    let names: Vec<(String, String)> = rows
        .iter()
        .map(|(s, p, _)| (s.clone(), p.clone()))
        .collect();
    assert_eq!(names, expected);
    assert!(
        rows.iter().filter(|(_, p, _)| p == "sub/gone.c").eq([&(
            "D".to_owned(),
            "sub/gone.c".to_owned(),
            Some(Staging::Staged)
        )]),
        "a file removed from the index but kept on disk is one staged deletion"
    );

    let oracle = git_staging(d);
    for (_, path, staging) in &rows {
        let path = path.trim_end_matches('/');
        assert_eq!(*staging, oracle.get(path).copied(), "{path}");
    }
    let of = |path: &str| rows.iter().find(|r| r.1 == path).and_then(|r| r.2);
    assert_eq!(of("staged.txt"), Some(Staging::Staged));
    assert_eq!(of("modified.txt"), Some(Staging::Unstaged));
    assert_eq!(of("new-untracked.txt"), Some(Staging::Unstaged));
    assert_eq!(of("staged-reverted.txt"), None, "back to HEAD: not listed");

    for p in ["sub", "newdir", "other/file.txt", "missing"] {
        let all: Vec<_> = rows
            .iter()
            .filter(|r| r.1 == p || r.1.starts_with(&format!("{p}/")))
            .cloned()
            .collect();
        assert_eq!(uncommitted(&repo, &[p]), all, "-- {p}");
    }
}

/// Staging a change keeps it listed, now as staged; editing it again makes
/// it partly staged.
#[test]
fn staged_changes_stay_listed_with_their_staging() {
    let dir = init();
    let d = dir.path();
    write(d, "a.txt", "a\n");
    write(d, "b.txt", "b\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    write(d, "a.txt", "a2\n");
    write(d, "c.txt", "c\n");
    std::fs::remove_file(d.join("b.txt")).expect("rm");
    let row = |s: &str, p: &str, staging| (s.to_owned(), p.to_owned(), Some(staging));

    let before = uncommitted(&Repo::discover(d).expect("repo"), &[]);
    assert_eq!(
        before,
        [
            row("M", "a.txt", Staging::Unstaged),
            row("D", "b.txt", Staging::Unstaged),
            row("??", "c.txt", Staging::Unstaged),
        ]
    );

    git(d, &["add", "-A"]);
    let repo = Repo::discover(d).expect("repo");
    assert_eq!(
        uncommitted(&repo, &[]),
        [
            row("M", "a.txt", Staging::Staged),
            row("D", "b.txt", Staging::Staged),
            row("A", "c.txt", Staging::Staged),
        ]
    );
    assert!(
        collect(&repo, &DiffSpec::IndexToWorktree { untracked: true }, &[]).is_empty(),
        "nothing left for git diff"
    );

    write(d, "a.txt", "a3\n");
    write(d, "c.txt", "c2\n");
    assert_eq!(
        uncommitted(&repo, &[]),
        [
            row("M", "a.txt", Staging::Partly),
            row("D", "b.txt", Staging::Staged),
            row("A", "c.txt", Staging::Partly),
        ]
    );
    let oracle = git_staging(d);
    assert_eq!(oracle.get("a.txt"), Some(&Staging::Partly));
    assert_eq!(oracle.get("c.txt"), Some(&Staging::Partly));
}

/// An index stopped mid-merge, built stage by stage with one path for each
/// conflict `git status` names: both modified (`uu`), both added (`aa`),
/// both deleted (`dd`, gone from disk), added by them (`ua`), added by us
/// (`au`, and `gone`, missing on disk), deleted by them (`ud`, back to its
/// committed content on disk).
fn conflicted_repo() -> tempfile::TempDir {
    let dir = init();
    let d = dir.path();
    for path in ["uu", "dd", "ud", "clean"] {
        write(d, path, "base\n");
    }
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    let blob = |content: &str| {
        write(d, "blob", content);
        git(d, &["hash-object", "-w", "blob"]).trim().to_owned()
    };
    let (base, ours, theirs) = (blob("base\n"), blob("ours\n"), blob("theirs\n"));
    std::fs::remove_file(d.join("blob")).expect("rm");
    git(d, &["rm", "-q", "--cached", "uu", "dd", "ud"]);
    let stages = [
        (&base, 1, "uu"),
        (&ours, 2, "uu"),
        (&theirs, 3, "uu"),
        (&ours, 2, "aa"),
        (&theirs, 3, "aa"),
        (&base, 1, "dd"),
        (&theirs, 3, "ua"),
        (&ours, 2, "au"),
        (&ours, 2, "gone"),
        (&base, 1, "ud"),
        (&ours, 2, "ud"),
    ];
    let mut child = Command::new("git")
        .arg("-C")
        .arg(d)
        .args(["update-index", "--index-info"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("git runs");
    let mut stdin = child.stdin.take().expect("stdin");
    for (id, stage, path) in stages {
        writeln!(stdin, "100644 {id} {stage}\t{path}").expect("feed stages");
    }
    drop(stdin);
    assert!(child.wait().expect("git exits").success());
    write(
        d,
        "uu",
        "<<<<<<< ours\nours\n||||||| base\nbase\n=======\ntheirs\n>>>>>>> theirs\n",
    );
    write(
        d,
        "aa",
        "<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\n",
    );
    std::fs::remove_file(d.join("dd")).expect("rm");
    write(d, "ua", "theirs\n");
    write(d, "au", "ours\n");
    dir
}

/// Every unmerged path is one `U` row in every diff that involves the
/// index, whatever its stages and working-tree file (git's plain diff adds
/// an `M` row for some, and `git diff HEAD` names none of them `U`), with
/// the working-tree file as its new side and both columns of `git status`
/// for its staging.
#[test]
fn unmerged_paths_are_listed_once_as_u() {
    let dir = conflicted_repo();
    let d = dir.path();
    let repo = Repo::discover(d).expect("repo");
    let unmerged = diff_name_status(d, &["--cached"]);
    assert_eq!(unmerged.len(), 7, "{unmerged:?}");
    let mut worktree = diff_name_status(d, &[]);
    worktree.dedup_by(|a, b| a.1 == b.1);
    assert_eq!(worktree, unmerged);

    let oracle = git_staging(d);
    let specs = [
        DiffSpec::IndexToWorktree { untracked: true },
        repo.resolve(&[], true, false).expect("spec"),
        repo.resolve(&["HEAD"], false, false).expect("spec"),
        repo.resolve(&[], false, false).expect("spec"),
    ];
    for spec in specs {
        let changes = collect(&repo, &spec, &[]);
        assert_eq!(ours(&changes), unmerged, "{spec:?}");
        for change in changes {
            if matches!(spec, DiffSpec::Uncommitted { .. }) {
                assert_eq!(
                    change.staging,
                    oracle.get(&change.path.to_string()).copied()
                );
            }
            assert_eq!(change.clone().reversed().status, Status::Unmerged);
            let fd = repo
                .file_diff(change, &Options::default())
                .expect("both sides load");
            if fd.change.path == "uu" {
                assert!(fd.new.bytes().starts_with(b"<<<<<<< ours\n"), "{spec:?}");
            }
        }
    }
}

/// Gives `path` an mtime its index entry doesn't have, so the scan has to
/// hash it to know whether it changed.
fn touch_old(dir: &Path, path: &str) {
    let file = std::fs::File::options()
        .write(true)
        .open(dir.join(path))
        .expect("open");
    let then = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    file.set_modified(then).expect("set mtime");
}

/// `git status --porcelain` of the worktree, without refreshing the index.
fn porcelain(dir: &Path) -> Vec<(String, String)> {
    status_worktree(&git(
        dir,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "-uall",
        ],
    ))
}

/// With `eol=crlf`, a checkout has CRLF lines while the blobs have LF. git
/// compares the file converted back, so a fresh checkout is clean, even
/// when every file has to be hashed; a changed file's diff shows git's
/// view of it, not a change on every line.
#[test]
fn line_ending_conversion_is_undone_before_comparing() {
    let dir = init();
    let d = dir.path();
    write(d, ".gitattributes", "*.txt text eol=crlf\n");
    write(d, "a.txt", "one\ntwo\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    std::fs::remove_file(d.join("a.txt")).expect("rm");
    git(d, &["checkout", "--", "a.txt"]);
    assert_eq!(
        std::fs::read(d.join("a.txt")).expect("read"),
        b"one\r\ntwo\r\n"
    );
    touch_old(d, "a.txt");

    let repo = Repo::discover(d).expect("repo");
    let spec = DiffSpec::IndexToWorktree { untracked: true };
    assert!(porcelain(d).is_empty(), "git calls it clean");
    assert_eq!(collect(&repo, &spec, &[]), [] as [FileChange; 0]);

    write(d, "a.txt", "one\r\nTWO\r\n");
    let changes = collect(&repo, &spec, &[]);
    assert_eq!(ours(&changes), [("M".to_owned(), "a.txt".to_owned())]);
    let fd = repo
        .file_diff(changes[0].clone(), &Options::default())
        .expect("diff");
    assert_eq!(fd.new.bytes(), b"one\nTWO\n");
    assert_eq!(fd.note, None);
    let stat = repo
        .stat(&changes[0], dffy_core::diff::Whitespace::Exact)
        .expect("stat");
    assert_eq!((stat.added, stat.removed), (1, 1));
}

/// A clean filter's output is what git stores, and what the scan compares
/// and the diff shows.
#[test]
fn clean_filters_apply_before_comparing() {
    let dir = init();
    let d = dir.path();
    git(d, &["config", "filter.upper.clean", "tr a-z A-Z"]);
    write(d, ".gitattributes", "*.up filter=upper\n");
    write(d, "f.up", "hello\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    assert_eq!(git(d, &["show", "HEAD:f.up"]), "HELLO\n");
    touch_old(d, "f.up");

    let repo = Repo::discover(d).expect("repo");
    let spec = DiffSpec::IndexToWorktree { untracked: true };
    assert!(porcelain(d).is_empty(), "git calls it clean");
    assert_eq!(collect(&repo, &spec, &[]), [] as [FileChange; 0]);

    write(d, "f.up", "hello\nworld\n");
    let changes = collect(&repo, &spec, &[]);
    assert_eq!(ours(&changes), porcelain(d));
    let fd = repo
        .file_diff(changes[0].clone(), &Options::default())
        .expect("diff");
    assert_eq!(fd.new.bytes(), b"HELLO\nWORLD\n");
}

/// A required filter that fails leaves the file as read from disk, and its
/// diff says so instead of failing.
#[test]
fn a_failing_filter_shows_the_file_as_on_disk_with_a_note() {
    let dir = init();
    let d = dir.path();
    write(d, "f.bin", "data\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    git(d, &["config", "filter.broken.clean", "exit 3"]);
    git(d, &["config", "filter.broken.required", "true"]);
    write(d, ".git/info/attributes", "*.bin filter=broken\n");
    write(d, "f.bin", "data\nmore\n");

    let repo = Repo::discover(d).expect("repo");
    let changes = collect(&repo, &DiffSpec::IndexToWorktree { untracked: false }, &[]);
    assert_eq!(ours(&changes), [("M".to_owned(), "f.bin".to_owned())]);
    let fd = repo
        .file_diff(changes[0].clone(), &Options::default())
        .expect("a failed filter isn't an error");
    assert_eq!(fd.new.bytes(), b"data\nmore\n");
    assert!(fd.note.is_some(), "the diff says why");
}

/// git never follows a symbolic link in the middle of a tracked path. With
/// `dir` replaced by a link to a directory outside the repository holding a
/// same-named file, `dir/file.txt` is deleted and `dir` untracked, and the
/// file outside is never read.
#[test]
fn a_symlinked_parent_directory_is_not_followed() {
    let dir = init();
    let d = dir.path();
    write(d, "dir/file.txt", "inside\n");
    write(d, "dir/other.txt", "same\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    let outside = tempfile::tempdir().expect("tempdir");
    write(outside.path(), "file.txt", "outside, and longer\n");
    write(outside.path(), "other.txt", "same\n");
    std::fs::remove_dir_all(d.join("dir")).expect("rm");
    std::os::unix::fs::symlink(outside.path(), d.join("dir")).expect("symlink");

    let repo = Repo::discover(d).expect("repo");
    let changes = collect(&repo, &DiffSpec::IndexToWorktree { untracked: true }, &[]);
    let sorted = |v: Vec<(String, String)>| v.into_iter().collect::<BTreeSet<_>>();
    assert_eq!(sorted(ours(&changes)), sorted(porcelain(d)));
    assert_eq!(
        sorted(ours(&changes)),
        sorted(vec![
            ("D".to_owned(), "dir/file.txt".to_owned()),
            ("D".to_owned(), "dir/other.txt".to_owned()),
            ("??".to_owned(), "dir".to_owned()),
        ])
    );

    let worktree = Side::Worktree {
        mode: Mode::File,
        id: None,
    };
    assert!(repo.load("dir/file.txt".into(), &worktree).is_err());
    assert_eq!(repo.on_disk("dir/file.txt".into()), None);
}

/// Before the first commit, `HEAD` is the empty tree: everything in the
/// index or the worktree is an addition.
#[test]
fn unborn_head_compares_against_the_empty_tree() {
    let dir = init();
    let d = dir.path();
    write(d, "staged.txt", "s\n");
    write(d, "dir/partly.txt", "p\n");
    git(d, &["add", "staged.txt", "dir/partly.txt"]);
    write(d, "dir/partly.txt", "p2\n");
    write(d, "untracked.txt", "u\n");
    let repo = Repo::discover(d).expect("repo");
    let row = |s: &str, p: &str, staging| (s.to_owned(), p.to_owned(), Some(staging));
    assert_eq!(
        uncommitted(&repo, &[]),
        [
            row("A", "dir/partly.txt", Staging::Partly),
            row("A", "staged.txt", Staging::Staged),
            row("??", "untracked.txt", Staging::Unstaged),
        ]
    );

    let staged = repo.resolve(&[], true, false).expect("staged spec");
    assert_eq!(
        ours(&collect(&repo, &staged, &[])),
        diff_name_status(d, &["--cached"])
    );
    let unstaged = repo
        .resolve_revisions(&Revisions {
            unstaged: true,
            ..Revisions::default()
        })
        .expect("unstaged spec");
    assert_eq!(
        ours(&collect(&repo, &unstaged, &[])),
        diff_name_status(d, &[])
    );
}

/// `--unstaged` is `git diff` exactly: the index against the worktree,
/// without untracked files. It takes no revisions.
#[test]
fn unstaged_matches_git_diff() {
    let dir = messy_repo();
    let d = dir.path();
    let repo = Repo::discover(d).expect("repo");
    let unstaged = Revisions {
        unstaged: true,
        ..Revisions::default()
    };
    let spec = repo.resolve_revisions(&unstaged).expect("spec");
    assert_eq!(spec, DiffSpec::IndexToWorktree { untracked: false });
    let changes = collect(&repo, &spec, &[]);
    assert_eq!(ours(&changes), diff_name_status(d, &[]));
    assert!(changes.iter().all(|c| c.staging.is_none()));
    for bad in [
        Revisions {
            revs: vec!["HEAD".to_owned()],
            ..unstaged.clone()
        },
        Revisions {
            staged: true,
            ..unstaged
        },
    ] {
        assert!(
            matches!(
                repo.resolve_revisions(&bad),
                Err(Error::UnsupportedRevision { .. })
            ),
            "{bad:?}"
        );
    }
}

#[test]
fn pathspecs_match_git_for_index_and_worktree() {
    let dir = messy_repo();
    let d = dir.path();
    let repo = Repo::discover(d).expect("repo");
    let staged = repo.resolve(&[], true, false).expect("staged spec");
    let head = repo.resolve(&["HEAD"], false, false).expect("head spec");
    let worktree = DiffSpec::IndexToWorktree { untracked: true };
    let sorted = |v: Vec<(String, String)>| v.into_iter().collect::<BTreeSet<_>>();
    for p in [
        "sub",
        "sub/",
        "newdir",
        "sub/dir/deep.c",
        "other",
        "missing",
    ] {
        let spec = p.trim_end_matches('/');
        assert_eq!(
            ours(&collect(&repo, &staged, &[p])),
            diff_name_status(d, &["--cached", "--", spec]),
            "--cached -- {p}"
        );
        assert_eq!(
            ours(&collect(&repo, &head, &[p])),
            diff_name_status(d, &["HEAD", "--", spec]),
            "HEAD -- {p}"
        );
        let expected = status_worktree(&git(
            d,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "-z",
                "-uall",
                "--",
                spec,
            ],
        ));
        assert_eq!(
            sorted(ours(&collect(&repo, &worktree, &[p]))),
            sorted(expected),
            "status -- {p}"
        );
    }
}

#[test]
fn honors_core_excludes_file() {
    let dir = init();
    let d = dir.path();
    write(d, "tracked.txt", "t\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    // Inside .git so the excludes file isn't itself untracked.
    let excludes = d.join(".git/my-excludes");
    std::fs::write(&excludes, "*.bak\n/top-only.txt\n").expect("excludes");
    git(
        d,
        &[
            "config",
            "core.excludesFile",
            excludes.to_str().expect("utf-8 path"),
        ],
    );
    for path in [
        "a.bak",
        "sub/b.bak",
        "top-only.txt",
        "sub/top-only.txt",
        "keep.txt",
    ] {
        write(d, path, "x\n");
    }

    let repo = Repo::discover(d).expect("repo");
    let got = ours(&collect(
        &repo,
        &DiffSpec::IndexToWorktree { untracked: true },
        &[],
    ));
    let expected = status_worktree(&git(
        d,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "-uall",
        ],
    ));
    assert_eq!(got, expected);
    let paths: Vec<_> = got.iter().map(|(_, p)| p.as_str()).collect();
    assert_eq!(
        paths,
        ["keep.txt", "sub/top-only.txt"],
        "the excludes file applied"
    );
}

#[test]
fn tree_to_tree_matches_git_diff() {
    let dir = init();
    let d = dir.path();
    write(d, "a.txt", "1\n");
    write(d, "dir/x.c", "x\n");
    write(d, "dir/y.c", "y\n");
    write(d, "file-then-dir", "f\n");
    write(d, "link", "plain\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "one"]);
    write(d, "a.txt", "2\n");
    std::fs::remove_file(d.join("dir/y.c")).expect("rm");
    write(d, "dir/z.c", "z\n");
    write(d, "dir.c", "sorts between dir and dir/\n");
    std::fs::remove_file(d.join("file-then-dir")).expect("rm");
    write(d, "file-then-dir/inner", "i\n");
    std::fs::remove_file(d.join("link")).expect("rm");
    std::os::unix::fs::symlink("a.txt", d.join("link")).expect("symlink");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "two"]);

    let repo = Repo::discover(d).expect("repo");
    for revs in [&["HEAD~1", "HEAD"][..], &["HEAD~1..HEAD"], &["HEAD^!"]] {
        let spec = repo.resolve(revs, false, false).expect("spec");
        assert_eq!(
            ours(&collect(&repo, &spec, &[])),
            diff_name_status(d, &["HEAD~1", "HEAD"]),
            "{revs:?}"
        );
    }
    let spec = repo
        .resolve(&["HEAD~1", "HEAD"], false, false)
        .expect("spec");
    assert_eq!(
        ours(&collect(&repo, &spec, &["dir"])),
        diff_name_status(d, &["HEAD~1", "HEAD", "--", "dir"]),
        "pathspec matches whole components"
    );
}

#[test]
fn racy_entries_are_hashed() {
    let dir = init();
    let d = dir.path();
    git(d, &["config", "core.trustctime", "false"]);
    write(d, "racy.txt", "aaaa\n");
    // An mtime in the future stays racy against any index write.
    let future = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    let set_mtime = |t| {
        std::fs::File::options()
            .write(true)
            .open(d.join("racy.txt"))
            .and_then(|f| f.set_modified(t))
            .expect("set mtime");
    };
    set_mtime(future);
    git(d, &["add", "racy.txt"]);
    git(d, &["commit", "-q", "-m", "racy"]);
    write(d, "racy.txt", "bbbb\n");
    set_mtime(future);

    let repo = Repo::discover(d).expect("repo");
    let changes = collect(&repo, &DiffSpec::IndexToWorktree { untracked: false }, &[]);
    assert_eq!(ours(&changes), [("M".to_owned(), "racy.txt".to_owned())]);
}

#[test]
fn stops_when_asked() {
    let dir = messy_repo();
    let repo = Repo::discover(dir.path()).expect("repo");
    let mut seen = 0;
    repo.for_each_change(&DiffSpec::IndexToWorktree { untracked: true }, &[], |_| {
        seen += 1;
        ControlFlow::Break(())
    })
    .expect("enumerates");
    assert_eq!(seen, 1);
}

/// root -> c2 (tag v1) -> c3 on main, and feature branching off c2.
fn history() -> (tempfile::TempDir, [ObjectId; 4]) {
    let dir = init();
    let d = dir.path();
    let commit = |msg: &str| {
        write(d, "f.txt", &format!("{msg}\n"));
        git(d, &["add", "-A"]);
        git(d, &["commit", "-q", "-m", msg]);
        git(d, &["rev-parse", "HEAD"])
            .trim()
            .parse::<ObjectId>()
            .expect("oid")
    };
    let root = commit("root");
    let c2 = commit("c2");
    git(d, &["tag", "v1"]);
    let c3 = commit("c3");
    git(d, &["switch", "-q", "-c", "feature", "v1"]);
    let f1 = commit("f1");
    git(d, &["switch", "-q", "main"]);
    (dir, [root, c2, c3, f1])
}

fn tree(dir: &Path, rev: &str) -> ObjectId {
    git(dir, &["rev-parse", &format!("{rev}^{{tree}}")])
        .trim()
        .parse()
        .expect("oid")
}

#[test]
fn resolves_git_diff_arguments() {
    let (dir, [root, c2, c3, f1]) = history();
    let d = dir.path();
    let repo = Repo::discover(d).expect("repo");
    let t = |id: ObjectId| tree(d, &id.to_string());
    let pair = |old, new| DiffSpec::TreeToTree { old, new };
    let resolve = |revs: &[&str]| repo.resolve(revs, false, false).expect("resolves");

    assert_eq!(resolve(&[]), DiffSpec::Uncommitted { head: t(c3) });
    assert_eq!(
        repo.resolve(&[], true, false).expect("staged"),
        DiffSpec::TreeToIndex { tree: t(c3) }
    );
    assert_eq!(
        repo.resolve(&["HEAD~2"], true, false).expect("staged rev"),
        DiffSpec::TreeToIndex { tree: t(root) }
    );
    assert_eq!(
        resolve(&["HEAD~1"]),
        DiffSpec::TreeToWorktree { tree: t(c2) }
    );
    assert_eq!(resolve(&["v1", "HEAD"]), pair(t(c2), t(c3)));
    assert_eq!(resolve(&["v1..main"]), pair(t(c2), t(c3)));
    assert_eq!(resolve(&["v1.."]), pair(t(c2), t(c3)));
    assert_eq!(resolve(&["main...feature"]), pair(t(c2), t(f1)));
    assert_eq!(resolve(&["HEAD^!"]), pair(t(c2), t(c3)));
    assert_eq!(resolve(&[&c2.to_string()[..7], "HEAD"]), pair(t(c2), t(c3)));
    assert_eq!(
        repo.resolve(&["feature"], false, true).expect("merge base"),
        DiffSpec::TreeToWorktree { tree: t(c2) }
    );
    assert_eq!(
        repo.resolve(&["main", "feature"], false, true)
            .expect("merge base pair"),
        pair(t(c2), t(f1))
    );

    git(
        d,
        &["update-ref", "refs/remotes/origin/main", &c2.to_string()],
    );
    git(
        d,
        &["config", "remote.origin.url", "https://example.invalid/x"],
    );
    git(
        d,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    );
    git(d, &["config", "branch.main.remote", "origin"]);
    git(d, &["config", "branch.main.merge", "refs/heads/main"]);
    let repo = Repo::discover(d).expect("repo");
    assert_eq!(
        repo.resolve(&["@{u}"], false, false).expect("upstream"),
        DiffSpec::TreeToWorktree { tree: t(c2) }
    );

    let empty: ObjectId = git(d, &["hash-object", "-t", "tree", "/dev/null"])
        .trim()
        .parse()
        .expect("oid");
    assert_eq!(
        repo.resolve_show("HEAD~2").expect("show root"),
        (root, pair(empty, t(root)))
    );
    assert_eq!(
        repo.resolve_show("v1").expect("show tag"),
        (c2, pair(t(root), t(c2)))
    );
}

#[test]
fn unknown_revisions_suggest_the_closest_ref() {
    let (dir, _) = history();
    let repo = Repo::discover(dir.path()).expect("repo");
    let suggestion = |rev: &str| match repo.resolve(&[rev], false, false) {
        Err(Error::UnknownRevision { suggestion, .. }) => suggestion,
        other => panic!("expected an unknown revision for {rev}, got {other:?}"),
    };
    assert_eq!(suggestion("amin").as_deref(), Some("main"));
    assert_eq!(suggestion("feaxture~1").as_deref(), Some("feature"));
    assert_eq!(suggestion("v2").as_deref(), Some("v1"));
    assert_eq!(suggestion("main..amin").as_deref(), Some("main"));
    assert_eq!(suggestion("completely-unrelated"), None);
    assert_eq!(
        suggestion("HEAD~40"),
        None,
        "the name resolves; the ancestry doesn't"
    );
}

#[test]
fn file_diff_loads_both_sides() {
    let dir = messy_repo();
    let repo = Repo::discover(dir.path()).expect("repo");
    let changes = collect(&repo, &DiffSpec::IndexToWorktree { untracked: true }, &[]);
    let find = |path: &str| {
        changes
            .iter()
            .find(|c| c.path == path)
            .cloned()
            .expect("change present")
    };

    let fd = repo
        .file_diff(find("modified.txt"), &Options::default())
        .expect("diff");
    assert_eq!(
        (fd.old.bytes(), fd.new.bytes()),
        (&b"old\n"[..], &b"new content\n"[..])
    );
    assert_eq!(fd.hunks.len(), 1);

    let link = find("to-link.txt");
    assert_eq!(link.status, Status::TypeChange);
    let fd = repo.file_diff(link, &Options::default()).expect("diff");
    assert_eq!(fd.new.bytes(), b"keep.txt", "symlinks read as their target");

    let exec = find("exec.sh");
    assert_eq!(exec.status, Status::Modified);
    assert_eq!(exec.new.map(|s| s.mode()), Some(Mode::Executable));
    let fd = repo.file_diff(exec, &Options::default()).expect("diff");
    assert_eq!(fd.hunks.len(), 0, "a mode-only change has no hunks");

    std::fs::write(dir.path().join("bin.dat"), b"\x00\x01binary").expect("write");
    let change = FileChange {
        path: "bin.dat".into(),
        status: Status::Untracked,
        old: None,
        new: Some(Side::Worktree {
            mode: Mode::File,
            id: None,
        }),
        staging: None,
    };
    let fd = repo.file_diff(change, &Options::default()).expect("diff");
    assert!(fd.binary && fd.hunks.is_empty());
}

/// A working-tree file compared with itself is read once and has no
/// hunks. It's found only along a path git could track there.
#[test]
fn an_unchanged_file_is_read_once_on_a_path_git_could_track() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = init();
    let d = dir.path();
    write(d, "src/a.rs", "fn a() {}\n");
    write(d, "run.sh", "#!/bin/sh\n");
    std::fs::set_permissions(d.join("run.sh"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    std::os::unix::fs::symlink(d.join("src"), d.join("link")).expect("symlink");
    let repo = Repo::discover(d).expect("repo");
    let root = repo.workdir().expect("a working tree");
    assert_eq!(worktree_file(root, b"src/a.rs"), Some(Mode::File));
    assert_eq!(worktree_file(root, b"run.sh"), Some(Mode::Executable));
    for refused in [
        "",
        "src",
        "gone.rs",
        "/etc/hosts",
        "../a.rs",
        "src/../src/a.rs",
        "./src/a.rs",
        "src//a.rs",
        ".git/config",
        ".GIT/config",
        "link/a.rs",
    ] {
        assert_eq!(worktree_file(root, refused.as_bytes()), None, "{refused}");
    }

    let side = Side::Worktree {
        mode: Mode::File,
        id: None,
    };
    let change = FileChange::unchanged("src/a.rs".into(), side);
    let fd = repo.file_diff(change, &Options::default()).expect("diff");
    assert_eq!(fd.hunks.len(), 0);
    assert_eq!(fd.new.bytes(), b"fn a() {}\n");
    assert_eq!(fd.old, fd.new);
}

#[test]
fn git_dir_is_the_worktrees_own() {
    let dir = init();
    write(dir.path(), "a.txt", "a\n");
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    let linked = dir.path().join("linked");
    let linked_path = linked.to_str().expect("utf-8 path");
    git(dir.path(), &["worktree", "add", "-q", linked_path]);
    for root in [dir.path(), &linked] {
        let expected = git(root, &["rev-parse", "--absolute-git-dir"]);
        let repo = Repo::discover(root).expect("repo");
        let git_dir = repo.git_dir().expect("a repository has one");
        assert_eq!(
            git_dir.canonicalize().expect("exists"),
            Path::new(expected.trim()).canonicalize().expect("exists")
        );
    }
    assert_ne!(
        Repo::discover(dir.path()).expect("repo").git_dir(),
        Repo::discover(&linked).expect("repo").git_dir()
    );
}

fn since_checkpoint(repo: &Repo) -> Vec<(String, String)> {
    let revisions = Revisions {
        since_checkpoint: Some(WhichCheckpoint::Latest),
        ..Revisions::default()
    };
    let spec = repo.resolve_revisions(&revisions).expect("resolves");
    assert!(matches!(spec, DiffSpec::Checkpoint { .. }), "{spec:?}");
    ours(&collect(repo, &spec, &[]))
}

/// What a checkpoint must leave as it found: the index's bytes, `HEAD`,
/// and what `git status` says of the working tree.
fn untouched(dir: &Path) -> (Vec<u8>, String, String) {
    (
        std::fs::read(dir.join(".git/index")).expect("index"),
        git(dir, &["rev-parse", "HEAD"]),
        git(
            dir,
            &["status", "--porcelain=v1", "-z", "-uall", "--ignored"],
        ),
    )
}

/// A checkpoint holds the files plain `dffy` lists, staged or not and
/// untracked, and none git ignores; right after one, nothing has changed
/// since it, and taking it wrote nothing but objects and its ref.
#[test]
fn a_checkpoint_holds_the_working_tree_as_plain_dffy_lists_it() {
    let dir = messy_repo();
    let d = dir.path();
    let repo = Repo::discover(d).expect("repo");
    let before = untouched(d);
    let checkpoint = repo.take_checkpoint().expect("checkpoint");
    assert_eq!(untouched(d), before);
    assert_eq!(checkpoint.number, 1);

    let tree = checkpoint.tree.to_string();
    let files = git(d, &["ls-tree", "-r", "--name-only", &tree]);
    let files: BTreeSet<&str> = files.lines().collect();
    for kept in [
        "modified.txt",
        "staged.txt",
        "sub/added.c",
        "sub/gone.c",
        "sub/untracked.c",
        "new-untracked.txt",
        "newdir/b/c.txt",
        "intent.txt",
        "to-link.txt",
    ] {
        assert!(files.contains(kept), "{kept} in {files:?}");
    }
    for left in ["deleted.txt", "debug.log", "build/out.o", "excluded.tmp"] {
        assert!(!files.contains(left), "{left} in {files:?}");
    }
    let blob = |path: &str| git(d, &["show", &format!("{tree}:{path}")]);
    assert_eq!(blob("modified.txt"), "new content\n");
    assert_eq!(blob("staged-reverted.txt"), "v1\n");
    let modes = git(d, &["ls-tree", &tree, "exec.sh", "to-link.txt"]);
    assert!(
        modes.contains("100755 blob") && modes.contains("120000 blob"),
        "{modes}"
    );

    assert_eq!(since_checkpoint(&repo), []);
}

/// Since a checkpoint: only what changed after it, a file untracked then
/// and edited since read as modified.
#[test]
fn since_a_checkpoint_lists_only_later_edits() {
    let dir = messy_repo();
    let d = dir.path();
    let repo = Repo::discover(d).expect("repo");
    repo.take_checkpoint().expect("checkpoint");
    write(d, "new-untracked.txt", "edited\n");
    write(d, "staged.txt", "v3\n");
    git(d, &["add", "staged.txt"]);
    std::fs::remove_file(d.join("keep.txt")).expect("rm");
    write(d, "later.txt", "new\n");
    write(d, "later.log", "ignored\n");
    let pair = |s: &str, p: &str| (s.to_owned(), p.to_owned());
    assert_eq!(
        since_checkpoint(&repo),
        [
            pair("D", "keep.txt"),
            pair("M", "new-untracked.txt"),
            pair("M", "staged.txt"),
            pair("??", "later.txt"),
        ]
    );
}

/// Watch mode's refresh of one path since a checkpoint pairs a file
/// untracked then the same way a full listing does: edited, it's
/// modified; written back as it was, it's gone.
#[test]
fn since_a_checkpoint_a_refresh_pairs_untracked_files_too() {
    let dir = messy_repo();
    let d = dir.path();
    let repo = Repo::discover(d).expect("repo");
    repo.take_checkpoint().expect("checkpoint");
    let revisions = Revisions {
        since_checkpoint: Some(WhichCheckpoint::Latest),
        ..Revisions::default()
    };
    let spec = repo.resolve_revisions(&revisions).expect("resolves");
    let mut set = ChangeSet::new(repo, spec, revisions, &[], false);
    set.enumerate(|_| ControlFlow::Continue(()))
        .expect("enumerates");
    assert_eq!(set.files(), []);
    let refresh = |set: &mut ChangeSet| {
        let update = set
            .refresh(&["new-untracked.txt".into()])
            .expect("refreshes");
        ours(&update.map(|u| u.files).unwrap_or_default())
    };
    write(d, "new-untracked.txt", "edited\n");
    assert_eq!(
        refresh(&mut set),
        [("M".to_owned(), "new-untracked.txt".to_owned())]
    );
    write(d, "new-untracked.txt", "hi\n");
    assert_eq!(refresh(&mut set), []);
}

/// A working-tree file too large to diff isn't copied into a checkpoint:
/// a tracked one keeps HEAD's version there, an untracked one is left out,
/// and both list as changed since it.
#[test]
fn a_checkpoint_leaves_out_files_too_large_to_diff() {
    let dir = init();
    let d = dir.path();
    write(d, "big.bin", "small at first\n");
    write(d, "a.txt", "a\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    let grow = |path: &str| {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(d.join(path))
            .expect("open");
        // Sparse: as large as it gets, without writing it.
        file.set_len(dffy_core::git::MAX_DIFF_BYTES).expect("grow");
    };
    grow("big.bin");
    grow("new-big.bin");
    write(d, "a.txt", "b\n");
    let repo = Repo::discover(d).expect("repo");
    let tree = repo.take_checkpoint().expect("checkpoint").tree.to_string();
    let files = git(d, &["ls-tree", "-r", "--name-only", &tree]);
    assert_eq!(files.lines().collect::<Vec<_>>(), ["a.txt", "big.bin"]);
    assert_eq!(
        git(d, &["show", &format!("{tree}:big.bin")]),
        "small at first\n"
    );
    assert_eq!(git(d, &["show", &format!("{tree}:a.txt")]), "b\n");
    let pair = |s: &str, p: &str| (s.to_owned(), p.to_owned());
    assert_eq!(
        since_checkpoint(&repo),
        [pair("M", "big.bin"), pair("??", "new-big.bin")]
    );
}

/// Each worktree keeps its own checkpoints, numbered on from its last,
/// the last five kept; asking for one that's gone, or before any, fails.
#[test]
fn checkpoints_keep_the_last_five_per_worktree() {
    let dir = init();
    let d = dir.path();
    write(d, "a.txt", "a\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    let repo = Repo::discover(d).expect("repo");
    let since = |which| Revisions {
        since_checkpoint: Some(which),
        ..Revisions::default()
    };
    assert!(matches!(
        repo.resolve_revisions(&since(WhichCheckpoint::Latest)),
        Err(Error::NoCheckpoint(None))
    ));
    for n in 1..=7 {
        write(d, "a.txt", &format!("{n}\n"));
        assert_eq!(repo.take_checkpoint().expect("checkpoint").number, n);
    }
    let numbers: Vec<u64> = (repo.checkpoints().expect("listed").iter())
        .map(|c| c.number)
        .collect();
    assert_eq!(numbers, [3, 4, 5, 6, 7]);
    assert!(matches!(
        repo.resolve_revisions(&since(WhichCheckpoint::Number(2))),
        Err(Error::NoCheckpoint(Some(2)))
    ));
    write(d, "a.txt", "8\n");
    assert_eq!(
        since_checkpoint(&repo),
        [("M".to_owned(), "a.txt".to_owned())]
    );
    let with_rev = Revisions {
        revs: vec!["HEAD".to_owned()],
        ..since(WhichCheckpoint::Latest)
    };
    assert!(matches!(
        repo.resolve_revisions(&with_rev),
        Err(Error::UnsupportedRevision { .. })
    ));

    let linked = d.join("linked");
    git(
        d,
        &["worktree", "add", "-q", linked.to_str().expect("utf-8")],
    );
    let other = Repo::discover(&linked).expect("repo");
    assert_eq!(other.checkpoints().expect("listed"), []);
    assert_eq!(other.take_checkpoint().expect("checkpoint").number, 1);
    assert_eq!(repo.checkpoints().expect("listed").len(), 5);
}

fn listed(repo: &Repo, at: Snapshot, filter: &[&str]) -> Vec<(String, Mode)> {
    let filter: Vec<_> = filter.iter().map(|p| (*p).into()).collect();
    let mut out = Vec::new();
    repo.list_paths(at, &filter, |path, mode| {
        out.push((path.to_string(), mode));
        ControlFlow::Continue(())
    })
    .expect("lists");
    out
}

fn names(listed: &[(String, Mode)]) -> Vec<&str> {
    listed.iter().map(|(path, _)| path.as_str()).collect()
}

/// A tree lists what `git ls-tree -r` does, the index what `git ls-files`
/// does but the intent-to-add files it holds no content for, and the
/// working tree every tracked path; all in git's order, and a filter keeps
/// whole components only.
#[test]
fn list_paths_lists_a_snapshot_in_git_order() {
    let dir = init();
    let d = dir.path();
    for path in ["sub-x", "sub.c", "sub/a", "sub/deep/b", "top.txt", "run.sh"] {
        write(d, path, "x\n");
    }
    std::os::unix::fs::symlink("top.txt", d.join("link")).expect("symlink");
    git(d, &["add", "-A"]);
    git(d, &["update-index", "--chmod=+x", "run.sh"]);
    git(d, &["commit", "-q", "-m", "base"]);
    write(d, "staged.c", "s\n");
    git(d, &["add", "staged.c"]);
    write(d, "intent.c", "i\n");
    git(d, &["add", "-N", "intent.c"]);
    std::fs::remove_file(d.join("top.txt")).expect("rm");
    let repo = Repo::discover(d).expect("repo");
    let head = repo.head_tree().expect("HEAD's tree");
    let z = |out: String| -> Vec<String> {
        out.split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };

    let tree = listed(&repo, Snapshot::Tree(head), &[]);
    assert_eq!(
        names(&tree),
        z(git(d, &["ls-tree", "-r", "-z", "--name-only", "HEAD"]))
    );
    assert!(tree.contains(&("run.sh".to_owned(), Mode::Executable)));
    assert!(tree.contains(&("link".to_owned(), Mode::Symlink)));
    let tracked = z(git(d, &["ls-files", "-z"]));
    assert_eq!(names(&listed(&repo, Snapshot::Worktree, &[])), tracked);
    let index: Vec<_> = tracked.iter().filter(|p| *p != "intent.c").collect();
    assert_eq!(names(&listed(&repo, Snapshot::Index, &[])), index);

    for at in [Snapshot::Tree(head), Snapshot::Index, Snapshot::Worktree] {
        assert_eq!(
            names(&listed(&repo, at, &["sub/"])),
            ["sub/a", "sub/deep/b"],
            "{at:?}"
        );
    }
    let mut first = Vec::new();
    repo.list_paths(Snapshot::Tree(head), &[], |path, _| {
        first.push(path.to_owned());
        ControlFlow::Break(())
    })
    .expect("lists");
    assert_eq!(first, ["link"]);
}

/// Listing with sides gives, for every path a snapshot lists, the side
/// `side_at` finds there: a blob for a tree or the index, the file on
/// disk with the index's mode for the working tree.
#[test]
fn list_sides_gives_what_side_at_finds() {
    let dir = init();
    let d = dir.path();
    for path in ["a.c", "sub/b.rs", "run.sh"] {
        write(d, path, "x\n");
    }
    git(d, &["add", "-A"]);
    git(d, &["update-index", "--chmod=+x", "run.sh"]);
    git(d, &["commit", "-q", "-m", "base"]);
    write(d, "a.c", "staged\n");
    git(d, &["add", "a.c"]);
    write(d, "a.c", "on disk\n");
    let repo = Repo::discover(d).expect("repo");
    let head = repo.head_tree().expect("HEAD's tree");
    for at in [Snapshot::Tree(head), Snapshot::Index, Snapshot::Worktree] {
        let mut sides = Vec::new();
        repo.list_sides(at, &[], |path, side| {
            sides.push((path.to_owned(), side));
            ControlFlow::Continue(())
        })
        .expect("lists");
        assert_eq!(sides.len(), 3, "{at:?}");
        for (path, side) in sides {
            let found = |at| repo.side_at(at, path.as_ref()).expect("reads");
            match at {
                Snapshot::Worktree => assert_eq!(
                    side,
                    Side::Worktree {
                        mode: found(Snapshot::Index).expect("indexed").mode(),
                        id: None
                    }
                ),
                _ => assert_eq!(Some(side), found(at), "{at:?} {path}"),
            }
        }
    }
}

/// A side as its mode and the bytes it loads, so the same content compares
/// equal from the index or the working tree.
fn content(repo: &Repo, path: &[u8], side: Option<Side>) -> Option<(Mode, Vec<u8>)> {
    side.map(|s| (s.mode(), repo.load(path.into(), &s).expect("loads")))
}

/// Each snapshot of a diff holds, at each of its paths, the content the
/// diff's own side there has, `-R` included; nothing where a side is
/// missing. Two documented exceptions: a submodule is nothing at any
/// snapshot, and the working tree has a file on disk that the diff lists
/// as deleted from git.
#[test]
fn side_at_a_diffs_snapshots_agrees_with_its_changes() {
    let dir = messy_repo();
    let d = dir.path();
    let repo = Repo::discover(d).expect("repo");
    let head = repo.head_tree().expect("HEAD's tree");
    let checkpoint = repo.take_checkpoint().expect("checkpoint").tree;
    write(d, "keep.txt", "changed since the checkpoint\n");
    let specs = [
        repo.resolve(&[], false, false).expect("spec"),
        DiffSpec::IndexToWorktree { untracked: true },
        repo.resolve(&[], true, false).expect("spec"),
        repo.resolve(&["HEAD"], false, false).expect("spec"),
        DiffSpec::TreeToTree {
            old: head,
            new: checkpoint,
        },
        DiffSpec::Checkpoint { tree: checkpoint },
    ];
    let (mut submodules, mut kept_on_disk) = (0, 0);
    for spec in specs {
        for reverse in [false, true] {
            let [old, new] = spec.snapshots(reverse).expect("a repository's diff");
            let changes = collect(&repo, &spec, &[]);
            assert!(!changes.is_empty(), "{spec:?}");
            for change in changes {
                let change = if reverse { change.reversed() } else { change };
                let gitlink = [change.old, change.new]
                    .iter()
                    .flatten()
                    .any(|s| s.mode() == Mode::Gitlink);
                submodules += usize::from(gitlink);
                let mut expected = |snapshot, path: &[u8], side: Option<Side>| {
                    if gitlink {
                        return None;
                    }
                    // `git rm --cached` left it on disk: the diff lists it
                    // as deleted, the working tree still has the file.
                    if path == b"sub/gone.c" && snapshot == Snapshot::Worktree {
                        kept_on_disk += usize::from(side.is_none());
                        return Some((Mode::File, b"int z;\n".to_vec()));
                    }
                    content(&repo, path, side)
                };
                let path = change.path.to_vec();
                let old_path = change.old_path().to_vec();
                let (want_old, want_new) = (
                    expected(old, &old_path, change.old),
                    expected(new, &path, change.new),
                );
                let at = |snapshot, path: &[u8]| {
                    let side = repo.side_at(snapshot, path.into()).expect("looks up");
                    content(&repo, path, side)
                };
                assert_eq!(
                    at(old, &old_path),
                    want_old,
                    "old side of {} in {spec:?}, reverse {reverse}",
                    change.path
                );
                assert_eq!(
                    at(new, &path),
                    want_new,
                    "new side of {} in {spec:?}, reverse {reverse}",
                    change.path
                );
            }
        }
    }
    assert!(submodules > 0 && kept_on_disk > 0);
    assert_eq!(DiffSpec::Paths.snapshots(false), None);
}

/// Folders, submodules and the files inside a submodule are nothing at
/// any snapshot, and a submodule isn't listed.
#[test]
fn only_whole_files_are_found_or_listed() {
    let dir = init();
    let d = dir.path();
    write(d, "dir/a.c", "a\n");
    let module = d.join("module");
    std::fs::create_dir(&module).expect("mkdir");
    git(&module, &["init", "-q"]);
    write(&module, "inner.txt", "x\n");
    git(&module, &["add", "-A"]);
    git(&module, &["commit", "-q", "-m", "inner"]);
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    let repo = Repo::discover(d).expect("repo");
    let head = repo.head_tree().expect("HEAD's tree");
    let side = |at, path: &str| repo.side_at(at, path.into()).expect("looks up");
    for at in [Snapshot::Tree(head), Snapshot::Index, Snapshot::Worktree] {
        for path in ["dir", "gone.c", "module", "module/inner.txt"] {
            assert_eq!(side(at, path), None, "{path} at {at:?}");
        }
        assert_eq!(names(&listed(&repo, at, &[])), ["dir/a.c"], "{at:?}");
    }
}

/// The index lists a conflicted path once, and its side and mode there are
/// the stage that stands for the index: ours, then the base, then theirs.
/// A diff of a tree with the index shows the working-tree file there
/// instead.
#[test]
fn the_index_holds_a_conflicted_path_once() {
    let dir = conflicted_repo();
    let d = dir.path();
    let base = git(d, &["rev-parse", ":1:uu"]);
    let mut child = Command::new("git")
        .arg("-C")
        .arg(d)
        .args(["update-index", "--index-info"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("git runs");
    let mut stdin = child.stdin.take().expect("stdin");
    writeln!(stdin, "100644 {} 1\tmode", base.trim()).expect("feed stages");
    writeln!(stdin, "100755 {} 2\tmode", base.trim()).expect("feed stages");
    drop(stdin);
    assert!(child.wait().expect("git exits").success());
    let repo = Repo::discover(d).expect("repo");

    let mut tracked: Vec<String> = git(d, &["ls-files", "-z"])
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    tracked.dedup();
    let index = listed(&repo, Snapshot::Index, &[]);
    assert_eq!(names(&index), tracked);
    assert!(index.contains(&("mode".to_owned(), Mode::Executable)));
    let stage = |spec: &str, mode| {
        let id = git(d, &["rev-parse", spec]);
        Some(Side::Object {
            id: ObjectId::from_hex(id.trim().as_bytes()).expect("hex"),
            mode,
        })
    };
    let side = |path: &str| {
        repo.side_at(Snapshot::Index, path.into())
            .expect("looks up")
    };
    assert_eq!(side("uu"), stage(":2:uu", Mode::File));
    assert_eq!(side("dd"), stage(":1:dd", Mode::File));
    assert_eq!(side("ua"), stage(":3:ua", Mode::File));
    assert_eq!(side("mode"), stage(":2:mode", Mode::Executable));

    let spec = repo.resolve(&[], true, false).expect("spec");
    assert_eq!(spec.snapshots(false).map(|s| s[1]), Some(Snapshot::Index));
    let uu = collect(&repo, &spec, &["uu"]);
    assert_eq!(uu.len(), 1);
    assert!(matches!(uu[0].new, Some(Side::Worktree { .. })), "{uu:?}");
}

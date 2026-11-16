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
use dffy_core::git::{DiffSpec, Error, FileChange, Mode, ObjectId, Repo, Revisions, Side, Status};
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

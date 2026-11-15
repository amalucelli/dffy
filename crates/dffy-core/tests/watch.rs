//! The watcher and the live change set together: edits on disk reach the
//! change set through the watcher, and after each step its file list is
//! what `git status` reports.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dffy_core::git::{ChangeSet, Repo, Revisions, Staging};
use dffy_core::watch::{Change, Notice, Root, Watcher};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).expect("utf-8 output")
}

fn write(dir: &Path, path: &str, content: &str) {
    let full = dir.join(path);
    std::fs::create_dir_all(full.parent().expect("has parent")).expect("mkdir");
    std::fs::write(full, content).expect("write");
}

/// What the default diff lists: paths `git diff HEAD` shows, and untracked
/// ones, each with its `git status` columns (`??` for untracked). Without
/// writing the index: that would reach the watcher as a git-state change and
/// re-list everything, hiding what a refresh got wrong.
fn git_status(dir: &Path) -> BTreeSet<String> {
    let changed: BTreeSet<String> = git(
        dir,
        &[
            "--no-optional-locks",
            "diff",
            "--no-ext-diff",
            "--name-only",
            "--no-renames",
            "HEAD",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect();
    let args = [
        "--no-optional-locks",
        "status",
        "--porcelain",
        "-uall",
        "--no-renames",
    ];
    git(dir, &args)
        .lines()
        .filter(|line| line.starts_with("??") || changed.contains(&line[3..]))
        .map(|line| format!("{} {}", columns(&line[..2]), &line[3..]))
        .collect()
}

/// `git status` columns as the staging they mean.
fn columns(xy: &str) -> &'static str {
    match xy.as_bytes() {
        b"??" | [b' ', _] => "unstaged",
        [_, b' '] => "staged",
        _ => "partly",
    }
}

fn listed(set: &ChangeSet) -> BTreeSet<String> {
    set.files()
        .iter()
        .map(|c| {
            let staging = match c.staging {
                Some(Staging::Staged) => "staged",
                Some(Staging::Unstaged) => "unstaged",
                Some(Staging::Partly) => "partly",
                None => return c.path.to_string(),
            };
            format!("{staging} {}", c.path)
        })
        .collect()
}

/// Applies notices to every set until none arrive for a while.
fn settle(rx: &mpsc::Receiver<Notice>, sets: &mut [ChangeSet], root: &Root) -> usize {
    let mut batches = 0;
    let start = Instant::now();
    while let Ok(notice) = rx.recv_timeout(Duration::from_millis(300)) {
        assert!(start.elapsed() < Duration::from_secs(10), "never settled");
        batches += 1;
        let (worktree, git_state) = match notice {
            Notice::Changed(paths) => {
                let mut worktree = Vec::new();
                let mut git_state = false;
                for path in &paths {
                    match root.classify(path) {
                        Some(Change::Worktree(rel)) => worktree.push(rel),
                        Some(Change::Git) => git_state = true,
                        None => panic!("the watcher passed on {}", path.display()),
                    }
                }
                (worktree, git_state)
            }
            Notice::Rescan => (Vec::new(), true),
            Notice::Polling(reason) => panic!("polling: {reason}"),
        };
        for set in sets.iter_mut() {
            if git_state {
                set.rescan().expect("rescan");
            } else {
                set.refresh(&worktree).expect("refresh");
            }
        }
    }
    batches
}

fn change_set(dir: &Path, revisions: Revisions) -> ChangeSet {
    let repo = Repo::discover(dir).expect("repo");
    let spec = repo.resolve_revisions(&revisions).expect("spec");
    let mut set = ChangeSet::new(repo, spec, revisions, &[], false);
    set.enumerate(|_| std::ops::ControlFlow::Continue(()))
        .expect("enumerate");
    set
}

#[test]
fn watched_edits_keep_the_change_set_equal_to_git_status() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    write(d, "src/a.rs", "fn a() {}\n");
    write(d, "src/b.rs", "fn b() {}\n");
    write(d, "README.md", "# demo\n");
    write(d, ".gitignore", "target/\n*.log\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);

    let root = Repo::discover(d).expect("repo").watch_root().expect("root");
    let mut set = [change_set(d, Revisions::default())];
    let (tx, rx) = mpsc::channel();
    let watcher = Watcher::start(root.clone(), move |n| tx.send(n).is_ok()).expect("watch");
    assert_eq!(
        watcher.backend(),
        if cfg!(target_os = "linux") {
            "inotify"
        } else {
            "fsevents"
        }
    );
    assert!(watcher.watches() > 0);

    let steps: [(&str, &dyn Fn()); 8] = [
        ("edit a file", &|| write(d, "src/a.rs", "fn a() { 1 }\n")),
        ("edit it again", &|| write(d, "src/a.rs", "fn a() { 2 }\n")),
        (
            "create files, some ignored, some in new directories",
            &|| {
                write(d, "target/out.o", "obj");
                write(d, "debug.log", "log");
                write(d, "new/deep/file.txt", "new");
                write(d, "notes.txt", "notes");
            },
        ),
        ("rename a directory", &|| {
            std::fs::rename(d.join("new"), d.join("renamed")).expect("rename");
        }),
        ("stage everything", &|| {
            git(d, &["add", "-A"]);
        }),
        ("delete and revert", &|| {
            std::fs::remove_file(d.join("src/b.rs")).expect("rm");
            write(d, "renamed/deep/file.txt", "changed");
            write(d, "src/a.rs", "fn a() { 2 }\n");
        }),
        ("create a scratch file", &|| {
            write(d, "renamed/deep/x.tmp", "tmp");
        }),
        ("ignore it from a nested .gitignore", &|| {
            write(d, "renamed/.gitignore", "*.tmp\n");
        }),
    ];
    for (name, step) in steps {
        step();
        assert!(settle(&rx, &mut set, &root) > 0, "{name}: no notice");
        assert_eq!(listed(&set[0]), git_status(d), "{name}");
    }
}

/// The next notice, if one comes within the settle time.
fn next_notice(rx: &mpsc::Receiver<Notice>) -> Option<Notice> {
    rx.recv_timeout(Duration::from_millis(300)).ok()
}

/// Directories under `dir`, itself included, as a watcher would count them.
#[cfg(target_os = "linux")]
fn dirs_below(dir: &Path) -> usize {
    1 + std::fs::read_dir(dir)
        .expect("read_dir")
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| dirs_below(&e.path()))
        .sum::<usize>()
}

/// The line between watched and skipped directories moves with
/// `.gitignore`, `info/exclude`, the global excludes file and the index.
/// After each step the change set equals `git status`, so nothing git lists
/// is ever dropped.
#[test]
fn ignored_untracked_directories_are_neither_watched_nor_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = &dir.path().canonicalize().expect("canonical");
    git(d, &["init", "-q", "-b", "main"]);
    write(d, ".gitignore", "target/\nbuild/*\n!build/keep/\n");
    // Anchored: `sub/gen` only, not `sub/x/gen`.
    write(d, "sub/.gitignore", "/gen/\n");
    write(d, "src/lib.rs", "fn f() {}\n");
    write(d, "target/keep/tracked.txt", "t\n");
    write(d, "build/keep/kept.txt", "k\n");
    write(d, "build/other/o.o", "o");
    write(d, "sub/gen/g.o", "g");
    write(d, "sub/x/gen/deep.txt", "d\n");
    for i in 0..40 {
        write(d, &format!("target/d{i}/sub/out.o"), "o");
    }
    git(d, &["add", "-A"]);
    git(d, &["add", "-f", "target/keep/tracked.txt"]);
    git(d, &["commit", "-q", "-m", "init"]);
    write(d, "notes/a.txt", "a");
    let global = tempfile::tempdir().expect("tempdir");
    let global_file = global.path().join("ignore");
    let global_path = global_file.to_str().expect("utf-8 path");
    git(d, &["config", "core.excludesFile", global_path]);

    let root = Repo::discover(d).expect("repo").watch_root().expect("root");
    let mut set = [change_set(d, Revisions::default())];
    let (tx, rx) = mpsc::channel();
    let watcher = Watcher::start(root.clone(), move |n| tx.send(n).is_ok()).expect("watch");
    // ., src, sub, sub/x, sub/x/gen, target, target/keep, build, build/keep,
    // notes; plus the git directory, info and refs.
    #[cfg(target_os = "linux")]
    let base = 10 + 2 + dirs_below(&d.join(".git/refs"));
    #[cfg(target_os = "linux")]
    assert_eq!(watcher.watches(), base, "ignored directories skipped");
    assert!(watcher.watches() > 0);

    write(d, "target/d3/sub/new.o", "o");
    write(d, "target/new-dir/x.o", "x");
    write(d, "build/other/n.o", "n");
    write(d, "sub/gen/n.o", "n");
    // Untracked, in an ignored directory watched for its tracked file.
    write(d, "target/keep/scratch.o", "s");
    assert_eq!(next_notice(&rx), None, "writes into ignored directories");
    #[cfg(target_os = "linux")]
    assert_eq!(
        watcher.watches(),
        base,
        "no watch on a new ignored directory"
    );

    let tracked = d.join("target/keep/tracked.txt");
    write(d, "target/keep/tracked.txt", "t2\n");
    assert_eq!(next_notice(&rx), Some(Notice::Changed(vec![tracked])));
    set[0]
        .refresh(&["target/keep/tracked.txt".into()])
        .expect("refresh");
    assert_eq!(
        listed(&set[0]),
        git_status(d),
        "a tracked file under target/"
    );

    write(d, ".gitignore", "build/*\n!build/keep/\n");
    assert_eq!(
        next_notice(&rx),
        Some(Notice::Rescan),
        "un-ignoring target/"
    );
    set[0].rescan().expect("rescan");
    settle(&rx, &mut set, &root);
    // d0..d39 and their sub/, and new-dir.
    #[cfg(target_os = "linux")]
    assert_eq!(watcher.watches(), base + 81, "target/ watched");
    write(d, "target/d7/sub/late.txt", "late");
    assert!(settle(&rx, &mut set, &root) > 0, "an edit in target/");
    assert!(listed(&set[0]).contains("unstaged target/d7/sub/late.txt"));
    assert_eq!(listed(&set[0]), git_status(d), "target/ un-ignored");

    write(d, ".git/info/exclude", "target/\n");
    assert_eq!(next_notice(&rx), Some(Notice::Rescan), "info/exclude");
    set[0].rescan().expect("rescan");
    #[cfg(target_os = "linux")]
    assert_eq!(watcher.watches(), base, "target/ excluded again");
    write(d, "target/d1/sub/z.o", "z");
    assert_eq!(next_notice(&rx), None, "a write into an excluded directory");
    assert_eq!(listed(&set[0]), git_status(d), "target/ excluded");

    git(d, &["add", "-f", "build/other/o.o"]);
    settle(&rx, &mut set, &root);
    #[cfg(target_os = "linux")]
    assert_eq!(watcher.watches(), base + 1, "build/other tracks a file now");
    write(d, "build/other/o.o", "o2");
    assert!(
        settle(&rx, &mut set, &root) > 0,
        "an edit to a force-added file"
    );
    assert_eq!(listed(&set[0]), git_status(d), "build/other/o.o staged");

    std::fs::write(&global_file, "notes/\n").expect("write");
    // The global file isn't watched; it's checked whenever a batch goes out.
    write(d, "src/lib.rs", "fn f() { 1 }\n");
    assert_eq!(
        next_notice(&rx),
        Some(Notice::Rescan),
        "the global excludes file"
    );
    set[0].rescan().expect("rescan");
    #[cfg(target_os = "linux")]
    assert_eq!(watcher.watches(), base, "notes/ excluded globally");
    write(d, "notes/b.txt", "b");
    assert_eq!(
        next_notice(&rx),
        None,
        "a write into a globally excluded directory"
    );
    assert_eq!(listed(&set[0]), git_status(d), "notes/ excluded globally");
}

/// Commits, staging and edits on a branch, watched by change sets for a
/// range, a revision against the working tree, and the staged changes: each
/// list follows what it compares as `HEAD` and the index move.
#[test]
fn watched_git_state_moves_what_revisions_compare() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    write(d, "a.txt", "a\n");
    write(d, "b.txt", "b\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    git(d, &["checkout", "-q", "-b", "feature"]);

    let root = Repo::discover(d).expect("repo").watch_root().expect("root");
    let revisions = |revs: &[&str], staged: bool| Revisions {
        revs: revs.iter().map(|r| (*r).to_owned()).collect(),
        staged,
        ..Revisions::default()
    };
    let mut sets = [
        change_set(d, revisions(&["main..HEAD"], false)),
        change_set(d, revisions(&["HEAD"], false)),
        change_set(d, revisions(&[], true)),
    ];
    let git_lists: [&[&str]; 3] = [
        &[
            "--no-optional-locks",
            "diff",
            "--name-only",
            "--no-renames",
            "main..HEAD",
        ],
        &[
            "--no-optional-locks",
            "diff",
            "--name-only",
            "--no-renames",
            "HEAD",
        ],
        &[
            "--no-optional-locks",
            "diff",
            "--name-only",
            "--no-renames",
            "--cached",
        ],
    ];
    let (tx, rx) = mpsc::channel();
    let _watcher = Watcher::start(root.clone(), move |n| tx.send(n).is_ok()).expect("watch");

    let steps: [(&str, &dyn Fn()); 5] = [
        ("edit", &|| write(d, "a.txt", "a2\n")),
        ("stage", &|| {
            git(d, &["add", "a.txt"]);
        }),
        ("commit", &|| {
            git(d, &["commit", "-q", "-m", "a2"]);
        }),
        ("edit and create", &|| {
            write(d, "b.txt", "b2\n");
            write(d, "c.txt", "c\n");
        }),
        ("commit everything", &|| {
            git(d, &["add", "-A"]);
            git(d, &["commit", "-q", "-m", "b2 c"]);
        }),
    ];
    for (name, step) in steps {
        step();
        settle(&rx, &mut sets, &root);
        for (set, args) in sets.iter().zip(git_lists) {
            let expected: BTreeSet<String> = git(d, args).lines().map(str::to_owned).collect();
            assert_eq!(listed(set), expected, "{name}: git {args:?}");
        }
    }
    assert_eq!(
        listed(&sets[0]).len(),
        3,
        "the range ends with every commit"
    );
}

/// A step of [`watched_staging_moves_each_files_staging`]: its name, what
/// it does, and the default diff's rows after it.
type Step<'a> = (&'a str, &'a dyn Fn(), &'a [&'a str]);

/// Staging, unstaging and commits reach the default diff through the
/// watcher as index changes: files stay listed while staged, and each
/// file's staging follows the index.
#[test]
fn watched_staging_moves_each_files_staging() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    write(d, "a.txt", "a\n");
    write(d, "b.txt", "b\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);

    let root = Repo::discover(d).expect("repo").watch_root().expect("root");
    let mut set = [change_set(d, Revisions::default())];
    let (tx, rx) = mpsc::channel();
    let _watcher = Watcher::start(root.clone(), move |n| tx.send(n).is_ok()).expect("watch");

    let steps: [Step; 9] = [
        ("edit", &|| write(d, "a.txt", "a2\n"), &["unstaged a.txt"]),
        (
            "stage",
            &|| drop(git(d, &["add", "a.txt"])),
            &["staged a.txt"],
        ),
        (
            "edit the staged file",
            &|| write(d, "a.txt", "a3\n"),
            &["partly a.txt"],
        ),
        (
            "stage again",
            &|| drop(git(d, &["add", "a.txt"])),
            &["staged a.txt"],
        ),
        (
            "unstage",
            &|| drop(git(d, &["reset", "-q", "a.txt"])),
            &["unstaged a.txt"],
        ),
        (
            "edit and create",
            &|| {
                write(d, "b.txt", "b2\n");
                write(d, "c.txt", "c\n");
            },
            &["unstaged a.txt", "unstaged b.txt", "unstaged c.txt"],
        ),
        (
            "stage the new file",
            &|| drop(git(d, &["add", "c.txt"])),
            &["unstaged a.txt", "unstaged b.txt", "staged c.txt"],
        ),
        (
            "stage everything",
            &|| drop(git(d, &["add", "-A"])),
            &["staged a.txt", "staged b.txt", "staged c.txt"],
        ),
        (
            "commit",
            &|| drop(git(d, &["commit", "-q", "-m", "all"])),
            &[],
        ),
    ];
    for (name, step, rows) in steps {
        step();
        settle(&rx, &mut set, &root);
        let expected: BTreeSet<String> = rows.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(listed(&set[0]), expected, "{name}");
        assert_eq!(git_status(d), expected, "{name}: git status agrees");
    }
}

/// Renames in a live change set: paired once after the enumeration, kept
/// through refreshes that don't touch the pair, and dropped when the pair
/// goes away.
#[test]
fn change_set_renames_follow_refreshes() {
    use dffy_core::git::Status;
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    let body = (0..20)
        .map(|i| format!("line {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    write(d, "old.txt", &body);
    write(d, "other.txt", "x\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    std::fs::remove_file(d.join("old.txt")).expect("rm");
    write(d, "new.txt", &body);
    git(d, &["add", "-N", "new.txt"]);

    let repo = Repo::discover(d).expect("repo");
    let revisions = Revisions::default();
    let spec = repo.resolve_revisions(&revisions).expect("spec");
    let mut set = ChangeSet::new(repo, spec, revisions, &[], false).with_renames(None);
    set.enumerate(|_| std::ops::ControlFlow::Continue(()))
        .expect("enumerate");
    let rows = |files: &[dffy_core::git::FileChange]| -> Vec<String> {
        files
            .iter()
            .map(|c| match &c.status {
                Status::Renamed { from, similarity } => format!("R{similarity} {from} {}", c.path),
                Status::Deleted => format!("D {}", c.path),
                Status::Modified => format!("M {}", c.path),
                s => format!("{s:?} {}", c.path),
            })
            .collect()
    };
    let update = set.renamed().expect("renames").expect("a pair");
    assert_eq!(rows(&update.files), ["R100 old.txt new.txt"]);

    write(d, "other.txt", "y\n");
    let update = set
        .refresh(&["other.txt".into()])
        .expect("refresh")
        .expect("changed");
    assert_eq!(rows(&update.files), ["R100 old.txt new.txt", "M other.txt"]);

    std::fs::remove_file(d.join("new.txt")).expect("rm");
    git(d, &["rm", "-q", "--cached", "new.txt"]);
    let update = set.rescan().expect("rescan").expect("changed");
    assert_eq!(rows(&update.files), ["D old.txt", "M other.txt"]);
}

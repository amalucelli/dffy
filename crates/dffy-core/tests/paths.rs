//! Comparing files and directories outside git through [`Repo::compare`]:
//! what's listed, in which order, and that each side loads from its own
//! path.
#![expect(
    clippy::expect_used,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::ops::ControlFlow;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use dffy_core::diff::Options;
use dffy_core::git::{
    ChangeSet, Compare, DiffSpec, Endpoint, FileChange, Mode, Repo, Revisions, Status,
};

fn write(dir: &Path, path: &str, content: impl AsRef<[u8]>) {
    let full = dir.join(path);
    std::fs::create_dir_all(full.parent().expect("has parent")).expect("mkdir");
    std::fs::write(full, content).expect("write");
}

fn compare(old: Endpoint, new: Endpoint, exclude: &[&str], gitignore: bool) -> Repo {
    Repo::compare(Compare {
        old,
        new,
        name: "name.txt".into(),
        exclude: exclude.iter().map(|&s| s.to_owned()).collect(),
        gitignore,
    })
    .expect("comparable")
}

fn dirs(old: &Path, new: &Path) -> Repo {
    compare(
        Endpoint::Path(old.to_owned()),
        Endpoint::Path(new.to_owned()),
        &[],
        false,
    )
}

fn changes(repo: &Repo) -> Vec<FileChange> {
    let mut out = Vec::new();
    repo.for_each_change(&DiffSpec::Paths, &[], |c| {
        out.push(c);
        ControlFlow::Continue(())
    })
    .expect("listed");
    out
}

/// `--name-status` lines, letters and paths.
fn listing(repo: &Repo) -> Vec<String> {
    changes(repo)
        .iter()
        .map(|c| {
            let letter = match c.status {
                Status::Added => "A",
                Status::Deleted => "D",
                Status::Modified => "M",
                Status::TypeChange => "T",
                Status::Untracked => "?",
                Status::Unmerged => "U",
                Status::Renamed { .. } => "R",
            };
            format!("{letter} {}", c.path)
        })
        .collect()
}

#[test]
fn two_directories_pair_files_by_path_in_git_order() {
    let (a, b) = (
        tempfile::tempdir().expect("a"),
        tempfile::tempdir().expect("b"),
    );
    let (a, b) = (a.path(), b.path());
    write(a, "same.txt", "same\n");
    write(b, "same.txt", "same\n");
    write(a, "gone.txt", "old\n");
    write(b, "new.txt", "new\n");
    write(a, "edit.c", "int a;\n");
    write(b, "edit.c", "int b;\n");
    write(a, "lib/deep/x.rs", "fn x() {}\n");
    write(b, "lib/deep/x.rs", "fn y() {}\n");
    write(b, "lib.c", "added next to a directory\n");
    write(a, "img.bin", b"\0\x01\x02");
    write(b, "img.bin", b"\0\x01\x03");
    // A file on one side and a directory on the other are unrelated paths.
    write(a, "turn", "file\n");
    write(b, "turn/inside", "dir\n");
    let repo = dirs(a, b);
    assert_eq!(
        listing(&repo),
        [
            "M edit.c",
            "D gone.txt",
            "M img.bin",
            "A lib.c",
            "M lib/deep/x.rs",
            "A new.txt",
            "D turn",
            "A turn/inside",
        ]
    );
    let all = changes(&repo);
    let fd = repo
        .file_diff(all[0].clone(), &Options::default())
        .expect("diffs");
    assert_eq!(fd.old.bytes(), b"int a;\n");
    assert_eq!(fd.new.bytes(), b"int b;\n");
    assert_eq!(fd.hunks.len(), 1);
    let binary = repo
        .file_diff(all[2].clone(), &Options::default())
        .expect("diffs");
    assert!(binary.binary);
    let reversed = repo
        .file_diff(all[0].clone().reversed(), &Options::default())
        .expect("diffs");
    assert_eq!(
        reversed.old.bytes(),
        b"int b;\n",
        "-R swaps which side loads"
    );
}

#[test]
fn a_mode_change_alone_is_a_change() {
    let (a, b) = (
        tempfile::tempdir().expect("a"),
        tempfile::tempdir().expect("b"),
    );
    write(a.path(), "run.sh", "echo\n");
    write(b.path(), "run.sh", "echo\n");
    let script = b.path().join("run.sh");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let all = changes(&dirs(a.path(), b.path()));
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].status, Status::Modified);
    assert_eq!(all[0].new.map(|s| s.mode()), Some(Mode::Executable));
}

/// `git difftool --dir-diff` links its working-tree side's files; a link
/// back up the tree isn't followed forever.
#[test]
fn symlinks_are_followed_and_cycles_are_not() {
    let (a, b, real) = (
        tempfile::tempdir().expect("a"),
        tempfile::tempdir().expect("b"),
        tempfile::tempdir().expect("real"),
    );
    write(a.path(), "src/main.rs", "fn a() {}\n");
    write(real.path(), "main.rs", "fn b() {}\n");
    std::fs::create_dir(b.path().join("src")).expect("mkdir");
    symlink(real.path().join("main.rs"), b.path().join("src/main.rs")).expect("link");
    symlink(b.path(), b.path().join("src/loop")).expect("link");
    symlink(b.path().join("missing"), b.path().join("dangling")).expect("link");
    let repo = dirs(a.path(), b.path());
    let all = changes(&repo);
    let names: Vec<String> = all.iter().map(|c| c.path.to_string()).collect();
    assert_eq!(names, ["dangling", "src/main.rs"]);
    assert_eq!(all[0].new.map(|s| s.mode()), Some(Mode::Symlink));
    let fd = repo
        .file_diff(all[1].clone(), &Options::default())
        .expect("diffs");
    assert_eq!(fd.new.bytes(), b"fn b() {}\n");
    assert_eq!(all[1].status, Status::Modified, "not a type change");
}

#[test]
fn exclude_patterns_and_gitignore_leave_paths_out() {
    let (a, b) = (
        tempfile::tempdir().expect("a"),
        tempfile::tempdir().expect("b"),
    );
    for (dir, tag) in [(a.path(), "a"), (b.path(), "b")] {
        write(dir, "keep.rs", tag);
        write(dir, "out.o", tag);
        write(dir, "target/debug/x", tag);
        write(dir, "src/gen/y", tag);
        write(dir, "deep/src/gen/z", tag);
        write(dir, "ignored.log", tag);
        write(dir, ".git/HEAD", tag);
        write(dir, ".gitignore", "*.log\n");
    }
    let everything = dirs(a.path(), b.path());
    assert_eq!(
        changes(&everything).len(),
        7,
        "nothing is left out by default"
    );
    let excluded = compare(
        Endpoint::Path(a.path().to_owned()),
        Endpoint::Path(b.path().to_owned()),
        &["*.o", "target/", "/src/gen"],
        false,
    );
    assert_eq!(
        listing(&excluded),
        [
            "M .git/HEAD",
            "M deep/src/gen/z",
            "M ignored.log",
            "M keep.rs"
        ]
    );
    let ignored = compare(
        Endpoint::Path(a.path().to_owned()),
        Endpoint::Path(b.path().to_owned()),
        &[],
        true,
    );
    assert_eq!(
        listing(&ignored),
        [
            "M deep/src/gen/z",
            "M keep.rs",
            "M out.o",
            "M src/gen/y",
            "M target/debug/x"
        ]
    );
}

#[test]
fn renames_pair_across_the_two_directories() {
    let (a, b) = (
        tempfile::tempdir().expect("a"),
        tempfile::tempdir().expect("b"),
    );
    let body = "line\n".repeat(40);
    write(a.path(), "old/name.txt", &body);
    write(b.path(), "new/name.txt", format!("{body}one more\n"));
    write(a.path(), "exact.rs", "fn same() {}\n");
    write(b.path(), "moved/exact.rs", "fn same() {}\n");
    let repo = dirs(a.path(), b.path());
    let paired = repo.detect_renames(changes(&repo), None).expect("renames");
    let found: Vec<(String, String)> = paired
        .iter()
        .map(|c| match &c.status {
            Status::Renamed { from, similarity } => {
                (format!("{from} -> {}", c.path), similarity.to_string())
            }
            other => (c.path.to_string(), format!("{other:?}")),
        })
        .collect();
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found.contains(&("exact.rs -> moved/exact.rs".to_owned(), "100".to_owned())),
        "{found:?}"
    );
    assert!(
        found
            .iter()
            .any(|(p, _)| p == "old/name.txt -> new/name.txt"),
        "{found:?}"
    );
    let renamed = paired
        .iter()
        .find(|c| c.path == "new/name.txt")
        .expect("renamed");
    let fd = repo
        .file_diff(renamed.clone(), &Options::default())
        .expect("diffs");
    assert_eq!(fd.hunks.len(), 1, "each side loads from its own directory");
}

#[test]
fn a_file_against_a_directory_takes_the_same_name_inside() {
    let (a, b) = (
        tempfile::tempdir().expect("a"),
        tempfile::tempdir().expect("b"),
    );
    write(a.path(), "conf.toml", "x = 1\n");
    write(b.path(), "conf.toml", "x = 2\n");
    let repo = compare(
        Endpoint::Path(a.path().join("conf.toml")),
        Endpoint::Path(b.path().to_owned()),
        &[],
        false,
    );
    let all = changes(&repo);
    assert_eq!(all.len(), 1);
    let fd = repo
        .file_diff(all[0].clone(), &Options::default())
        .expect("diffs");
    assert_eq!(fd.new.bytes(), b"x = 2\n");
    let missing = Repo::compare(Compare {
        old: Endpoint::Path(a.path().join("conf.toml")),
        new: Endpoint::Path(a.path().join("nowhere")),
        name: "conf.toml".into(),
        exclude: Vec::new(),
        gitignore: false,
    });
    assert!(missing.is_err());
}

#[test]
fn stdin_bytes_and_dev_null_are_files() {
    let dir = tempfile::tempdir().expect("dir");
    write(dir.path(), "b.txt", "one\ntwo\n");
    let file = || Endpoint::Path(dir.path().join("b.txt"));
    let same = compare(Endpoint::Bytes(b"one\ntwo\n".to_vec()), file(), &[], false);
    assert!(changes(&same).is_empty(), "identical content is no change");
    let edited = compare(Endpoint::Bytes(b"one\n".to_vec()), file(), &[], false);
    let all = changes(&edited);
    assert_eq!(listing(&edited), ["M name.txt"]);
    let fd = edited
        .file_diff(all[0].clone(), &Options::default())
        .expect("diffs");
    assert_eq!(fd.old.bytes(), b"one\n");
    let added = compare(Endpoint::Missing, file(), &[], false);
    assert_eq!(listing(&added), ["A name.txt"]);
    let nothing = compare(Endpoint::Missing, Endpoint::Missing, &[], false);
    assert!(
        listing(&nothing).is_empty(),
        "two missing files are no change"
    );
    let against_dir = Repo::compare(Compare {
        old: Endpoint::Bytes(Vec::new()),
        new: Endpoint::Path(dir.path().to_owned()),
        name: "x".into(),
        exclude: Vec::new(),
        gitignore: false,
    });
    assert!(against_dir.is_err());
}

/// Watch mode re-lists only the paths that changed.
#[test]
fn a_path_filter_limits_the_walk() {
    let (a, b) = (
        tempfile::tempdir().expect("a"),
        tempfile::tempdir().expect("b"),
    );
    write(a.path(), "x/1", "a");
    write(b.path(), "x/1", "b");
    write(a.path(), "y/2", "a");
    write(b.path(), "y/2", "b");
    let repo = dirs(a.path(), b.path());
    let mut found = Vec::new();
    repo.for_each_change(&DiffSpec::Paths, &["y".into()], |c| {
        found.push(c.path.to_string());
        ControlFlow::Continue(())
    })
    .expect("listed");
    assert_eq!(found, ["y/2"]);
}

#[test]
fn an_edit_to_a_renames_old_side_makes_it_dirty() {
    let (a, b) = (
        tempfile::tempdir().expect("a"),
        tempfile::tempdir().expect("b"),
    );
    let body = "line\n".repeat(40);
    write(a.path(), "from.txt", &body);
    write(b.path(), "to.txt", format!("{body}more\n"));
    let repo = dirs(a.path(), b.path());
    let mut set =
        ChangeSet::new(repo, DiffSpec::Paths, Revisions::default(), &[], false).with_renames(None);
    set.enumerate(|_| ControlFlow::Continue(()))
        .expect("enumerate");
    let paired = set.renamed().expect("renames").expect("a pair");
    let renamed = paired.files[0].clone();
    assert!(
        matches!(renamed.status, Status::Renamed { .. }),
        "{renamed:?}"
    );
    write(a.path(), "from.txt", format!("{body}edited\n"));
    let update = set
        .refresh(&["from.txt".into()])
        .expect("refresh")
        .expect("an update");
    assert!(update.is_dirty(&renamed));
}

/// A pipe, as from `<(command)`, stats as empty; it's read whole instead.
#[test]
fn a_pipe_is_read_as_its_content() {
    let dir = tempfile::tempdir().expect("dir");
    write(dir.path(), "b.txt", "one\ntwo\n");
    let fifo = dir.path().join("fifo");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo runs");
    assert!(made.success());
    let writer = {
        let fifo = fifo.clone();
        std::thread::spawn(move || std::fs::write(fifo, "one\n").expect("write the pipe"))
    };
    let repo = compare(
        Endpoint::Path(fifo),
        Endpoint::Path(dir.path().join("b.txt")),
        &[],
        false,
    );
    writer.join().expect("writer");
    let all = changes(&repo);
    assert_eq!(listing(&repo), ["M name.txt"]);
    let fd = repo
        .file_diff(all[0].clone(), &Options::default())
        .expect("diffs");
    assert_eq!(fd.old.bytes(), b"one\n");
}

/// A file one side can't read doesn't stop the walk: it's listed, as it
/// may differ, and reading it names the side that failed.
#[test]
fn an_unreadable_file_is_listed_and_named_and_the_walk_goes_on() {
    let (a, b) = (
        tempfile::tempdir().expect("a"),
        tempfile::tempdir().expect("b"),
    );
    let (a, b) = (a.path(), b.path());
    for dir in [a, b] {
        write(dir, "locked.txt", "same\n");
    }
    write(a, "z.txt", "old\n");
    write(b, "z.txt", "new\n");
    let locked = b.join("locked.txt");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    if std::fs::File::open(&locked).is_ok() {
        // Run as root: nothing is unreadable.
        return;
    }
    let repo = dirs(a, b);
    assert_eq!(listing(&repo), ["M locked.txt", "M z.txt"]);
    let change = changes(&repo).remove(0);
    let new = change.new.expect("a new side");
    let error = repo
        .load(change.path.as_ref(), &new)
        .expect_err("unreadable");
    assert!(
        error.to_string().contains(&*locked.to_string_lossy()),
        "{error}"
    );
    let old = change.old.expect("an old side");
    assert!(repo.load(change.path.as_ref(), &old).is_ok());
}

/// A tree as deep as paths allow is walked without running out of stack.
#[test]
fn deep_trees_are_walked() {
    let (a, b) = (
        tempfile::tempdir().expect("a"),
        tempfile::tempdir().expect("b"),
    );
    // macOS allows 1024 bytes for a whole path, NUL included, where Linux
    // allows 4096 and fits all 1500 levels.
    let limit = if cfg!(target_os = "macos") {
        1024
    } else {
        4096
    };
    // Resolved, since macOS counts the target of a symlink on the way,
    // `/var` to `/private/var`, against the limit.
    let resolved = |p: &Path| {
        std::fs::canonicalize(p)
            .expect("canonical")
            .as_os_str()
            .len()
    };
    let root = resolved(a.path()).max(resolved(b.path()));
    let levels = ((limit - 1 - root - "/f.txt".len()) / 2).min(1500);
    let deep = "d/".repeat(levels);
    write(a.path(), &format!("{deep}f.txt"), "old\n");
    write(b.path(), &format!("{deep}f.txt"), "new\n");
    assert_eq!(
        listing(&dirs(a.path(), b.path())),
        [format!("M {deep}f.txt")]
    );
}

//! `dffy A B` on files and folders outside git, end to end: printed
//! output against `diff -u`, summaries, exit codes, how arguments are told
//! apart from revisions inside a repository, and `git difftool`.
#![expect(
    clippy::expect_used,
    reason = "fixture helpers fail the test at the step that broke"
)]

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Runs dffy in `dir` with every XDG directory inside `dir`, so the user's
/// config, state, cache and broker are never touched.
fn dffy_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dffy"));
    cmd.current_dir(dir)
        .args(args)
        .env("XDG_CONFIG_HOME", dir.join(".xdg/config"))
        .env("XDG_CACHE_HOME", dir.join(".xdg/cache"))
        .env("XDG_STATE_HOME", dir.join(".xdg/state"))
        .env("XDG_RUNTIME_DIR", dir.join(".xdg"))
        .env("PATH", common::path_without_review_plugins())
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("COLORTERM")
        .env_remove("DFFY_PAGER")
        .env_remove("COLUMNS")
        .env_remove("GIT_DIFF_PATH_COUNTER")
        .env_remove("GIT_DIFF_PATH_TOTAL")
        .env_remove("GIT_DIFFTOOL_DIRDIFF");
    cmd
}

fn dffy(dir: &Path, args: &[&str]) -> Output {
    dffy_cmd(dir, args)
        .stdin(Stdio::null())
        .output()
        .expect("spawn dffy")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("utf-8 stdout")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn write(dir: &Path, path: &str, content: impl AsRef<[u8]>) {
    let full = dir.join(path);
    std::fs::create_dir_all(full.parent().expect("has parent")).expect("mkdir");
    std::fs::write(full, content).expect("write");
}

fn git(dir: &Path, args: &[&str], home: &Path) -> Output {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", home.join("gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada Lovelace")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .env("XDG_CONFIG_HOME", dir.join(".xdg/config"))
        .env("XDG_CACHE_HOME", dir.join(".xdg/cache"))
        .env("XDG_STATE_HOME", dir.join(".xdg/state"))
        .env("XDG_RUNTIME_DIR", dir.join(".xdg"))
        .env_remove("COLORTERM")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", stderr(&out));
    out
}

/// The removed and added lines of a diff, in order: `diff -u`'s with their
/// signs, dffy's from the sign column of its classic unified output.
fn changed_lines(text: &str, dffy: bool) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            if dffy {
                let (_, code) = line.split_once('│')?;
                code.starts_with(['-', '+']).then(|| code.to_owned())
            } else {
                let changed = line.starts_with(['-', '+'])
                    && !line.starts_with("---")
                    && !line.starts_with("+++");
                changed.then(|| line.to_owned())
            }
        })
        .collect()
}

#[test]
fn two_files_print_the_same_changes_as_diff_u() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    let lines: Vec<String> = (1..=60).map(|i| format!("line {i}\n")).collect();
    let old = lines.concat();
    let mut new = lines;
    new[4] = "line five\n".to_owned();
    new.insert(30, "inserted\n".to_owned());
    new.remove(50);
    write(d, "a.txt", &old);
    write(d, "b.txt", new.concat());
    let out = dffy(d, &["--color=never", "--unified", "a.txt", "b.txt"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let expected = Command::new("diff")
        .current_dir(d)
        .args(["-u", "a.txt", "b.txt"])
        .output()
        .expect("diff runs");
    let ours = changed_lines(&stdout(&out), true);
    assert_eq!(ours, changed_lines(&stdout(&expected), false));
    assert!(
        stdout(&out).starts_with("modified: b.txt\n"),
        "{}",
        stdout(&out)
    );

    // diff -u's counts: one line changed, one inserted, one removed.
    assert_eq!(ours.len(), 4, "{ours:?}");
    let stat = dffy(d, &["--stat", "a.txt", "b.txt"]);
    assert_eq!(
        stdout(&stat),
        " b.txt | 4 ++--\n 1 file changed, 2 insertions(+), 2 deletions(-)\n"
    );

    let differ = dffy(d, &["--exit-code", "--name-status", "a.txt", "b.txt"]);
    assert_eq!(differ.status.code(), Some(1));
    assert_eq!(stdout(&differ), "M\tb.txt\n");
    let same = dffy(d, &["--exit-code", "a.txt", "a.txt"]);
    assert_eq!(same.status.code(), Some(0), "{}", stderr(&same));
    assert_eq!(stdout(&same), "");
}

#[test]
fn two_folders_list_added_deleted_modified_binary_and_nested_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    write(d, "old/same.txt", "same\n");
    write(d, "new/same.txt", "same\n");
    write(d, "old/gone.txt", "bye\n");
    write(d, "new/sub/deeper/added.rs", "fn new() {}\n");
    write(d, "old/src/lib.rs", "fn a() {}\n");
    write(d, "new/src/lib.rs", "fn b() {}\n");
    write(d, "old/logo.png", b"\x89PNG\0\x01");
    write(d, "new/logo.png", b"\x89PNG\0\x02\x03");
    let out = dffy(d, &["--name-status", "old", "new/"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "D\tgone.txt\nM\tlogo.png\nM\tsrc/lib.rs\nA\tsub/deeper/added.rs\n"
    );
    let stat = stdout(&dffy(d, &["--stat", "old", "new"]));
    assert!(
        stat.contains("logo.png            | Bin 6 -> 7 bytes"),
        "{stat}"
    );
    assert!(
        stat.contains("4 files changed, 2 insertions(+), 2 deletions(-)"),
        "{stat}"
    );
    let printed = stdout(&dffy(d, &["--color=never", "--unified", "old", "new"]));
    assert!(printed.contains("binary files differ"), "{printed}");
    assert!(
        printed.contains("new file: sub/deeper/added.rs"),
        "{printed}"
    );
}

#[test]
fn exclude_leaves_paths_out_and_renames_pair_across_folders() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    let body = (0..30)
        .map(|i| format!("row {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    write(d, "a/docs/guide.md", &body);
    write(d, "b/manual/guide.md", format!("{body}row 30\n"));
    write(d, "a/target/out.o", "1");
    write(d, "b/target/out.o", "2");
    write(d, "a/x.log", "1");
    write(d, "b/x.log", "2");
    let out = dffy(
        d,
        &[
            "--name-status",
            "--exclude",
            "target/",
            "--exclude",
            "*.log",
            "a",
            "b",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "R096\tdocs/guide.md\tmanual/guide.md\n");
    let unpaired = dffy(
        d,
        &[
            "--name-status",
            "--no-renames",
            "--exclude=target",
            "a",
            "b",
        ],
    );
    assert_eq!(
        stdout(&unpaired),
        "D\tdocs/guide.md\nA\tmanual/guide.md\nM\tx.log\n"
    );
    let misplaced = dffy(d, &["--exclude", "x", "--staged"]);
    assert_eq!(misplaced.status.code(), Some(128));
    assert!(
        stderr(&misplaced).contains("only when comparing folders"),
        "{}",
        stderr(&misplaced)
    );
}

#[test]
fn a_file_against_a_folder_and_stdin_against_a_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    write(d, "conf.toml", "x = 1\n");
    write(d, "backup/conf.toml", "x = 2\n");
    let out = dffy(d, &["--color=never", "--unified", "conf.toml", "backup"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("-x = 1") && stdout(&out).contains("+x = 2"),
        "{}",
        stdout(&out)
    );
    let itself = dffy(d, &["conf.toml", "."]);
    assert!(
        itself.status.success(),
        "the folder's own conf.toml: {}",
        stderr(&itself)
    );
    assert_eq!(stdout(&itself), "");
    let missing = dffy(d, &["conf.toml", "nowhere.toml"]);
    assert_eq!(missing.status.code(), Some(128));
    assert!(
        stderr(&missing).contains("'nowhere.toml': no such file or directory"),
        "{}",
        stderr(&missing)
    );

    let mut child = dffy_cmd(d, &["--color=never", "--unified", "-", "conf.toml"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"x = 0\n")
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.starts_with("modified: conf.toml\n"),
        "named after the file: {text}"
    );
    assert!(text.contains("-x = 0") && text.contains("+x = 1"), "{text}");
}

#[test]
fn inside_a_repository_revisions_win_and_ambiguity_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"], d);
    write(d, "main", "a file named like the branch\n");
    write(d, "notes.txt", "notes\n");
    write(d, "other.txt", "other\n");
    git(d, &["add", "-A"], d);
    git(d, &["commit", "-q", "-m", "init"], d);
    git(d, &["branch", "dev"], d);
    write(d, "dev", "and one like the other branch\n");

    let ambiguous = dffy(d, &["main", "notes.txt"]);
    assert_eq!(ambiguous.status.code(), Some(128));
    assert!(
        stderr(&ambiguous)
            .contains("'main' is both a file and a revision; use --no-index to compare files"),
        "{}",
        stderr(&ambiguous)
    );
    let forced = dffy(d, &["--no-index", "--name-status", "main", "notes.txt"]);
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert_eq!(stdout(&forced), "M\tnotes.txt\n");

    // Both are revisions too: git's preference, revisions.
    let revisions = dffy(d, &["--name-status", "main", "dev"]);
    assert!(revisions.status.success(), "{}", stderr(&revisions));
    assert_eq!(stdout(&revisions), "", "main and dev are the same commit");

    // Neither is a revision: the files.
    let files = dffy(d, &["--name-status", "notes.txt", "other.txt"]);
    assert_eq!(stdout(&files), "M\tother.txt\n");

    // After `--`, paths inside the working tree stay pathspecs.
    write(d, "notes.txt", "notes, edited\n");
    let pathspecs = dffy(d, &["--name-status", "--", "notes.txt", "other.txt"]);
    assert_eq!(stdout(&pathspecs), "M\tnotes.txt\n");

    // One path outside the working tree: compared directly.
    let outside = tempfile::tempdir().expect("outside");
    write(outside.path(), "notes.txt", "notes, edited\n");
    let theirs = outside.path().join("notes.txt");
    let out = dffy(
        d,
        &["--exit-code", "notes.txt", theirs.to_str().expect("utf-8")],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

/// `git difftool --tool=dffy`, set up as the docs say: each file arrives as
/// a temporary copy, named and highlighted after the real file; with
/// `--dir-diff`, two folders whose working-tree side links to the files.
#[test]
fn difftool_names_and_highlights_temporary_copies() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    let config = format!(
        "[difftool \"dffy\"]\n\tcmd = {} --color=always --unified \\\"$LOCAL\\\" \\\"$REMOTE\\\"\n\
         [difftool]\n\tprompt = false\n",
        env!("CARGO_BIN_EXE_dffy")
    );
    std::fs::write(d.join("gitconfig"), config).expect("gitconfig");
    let repo = d.join("repo");
    std::fs::create_dir(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"], d);
    // Shaped like an older git's `XXXXXX_<name>` temporary name, which it
    // isn't.
    let file = "src/server_main.rs";
    write(&repo, file, "fn main() {\n    let x = 1;\n}\n");
    git(&repo, &["add", "-A"], d);
    git(&repo, &["commit", "-q", "-m", "init"], d);
    write(&repo, file, "fn main() {\n    let x = 2;\n}\n");
    git(&repo, &["commit", "-qam", "two"], d);

    // Both sides are temporary copies (/tmp/git-blob-XXXXXX/server_main.rs).
    let out = git(&repo, &["difftool", "--tool=dffy", "HEAD~1", "HEAD"], d);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("modified: server_main.rs"), "{text}");
    assert!(
        !text.contains("git-blob"),
        "the temporary name stays hidden: {text}"
    );
    // Rust highlighting: `let` gets a color of its own, unlike the plain
    // text around it.
    assert!(
        text.contains("m let\x1b") || text.contains("mlet\x1b"),
        "{text:?}"
    );

    // The working tree as one side: named by its path there.
    write(&repo, file, "fn main() {\n    let x = 3;\n}\n");
    let out = git(&repo, &["difftool", "--tool=dffy"], d);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("modified: src/server_main.rs"), "{text}");

    let out = git(&repo, &["difftool", "--tool=dffy", "-d", "HEAD~1"], d);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("modified: src/server_main.rs"),
        "followed, not a type change: {text}"
    );
}

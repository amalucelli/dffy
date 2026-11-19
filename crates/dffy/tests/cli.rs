//! The `dffy` binary end to end: print mode on fixture repositories, pager
//! mode on stdin, exit codes, errors, and the informational subcommands.
#![expect(
    clippy::expect_used,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const FIXTURE: &[u8] = include_bytes!("fixtures/git-diff.diff");

/// Runs dffy in `dir` with an empty config directory and no color
/// variables, so the user's environment can't leak into results.
fn dffy(dir: &Path, args: &[&str]) -> Output {
    dffy_cmd(dir, args).output().expect("spawn dffy")
}

fn dffy_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dffy"));
    cmd.current_dir(dir)
        .args(args)
        .env("XDG_CONFIG_HOME", dir.join(".no-config"))
        // Inside the git directory, so the cache never shows as a change.
        .env("XDG_CACHE_HOME", dir.join(".git/dffy-test-cache"))
        .env("XDG_STATE_HOME", dir.join(".git/dffy-test-state"))
        // A broker of the user's own is never asked or started.
        .env("XDG_RUNTIME_DIR", dir)
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        // Decides the default indicators.
        .env_remove("COLORTERM")
        .env_remove("DFFY_PAGER")
        .env_remove("COLUMNS");
    cmd
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("utf-8 stdout")
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).expect("utf-8 stderr")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada Lovelace")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .env("GIT_AUTHOR_DATE", "2024-03-01T12:00:00+0100")
        .env("GIT_COMMITTER_DATE", "2024-03-01T12:00:00+0100")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", stderr(&out));
    stdout(&out)
}

fn write(dir: &Path, path: &str, content: &str) {
    let full = dir.join(path);
    std::fs::create_dir_all(full.parent().expect("has parent")).expect("mkdir");
    std::fs::write(full, content).expect("write");
}

/// A repository with two commits on `main` and a clean worktree.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    write(
        d,
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    );
    write(d, "README.md", "# demo\n");
    write(d, "old.txt", "remove me\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "Initial commit"]);
    write(
        d,
        "src/lib.rs",
        "pub fn add(a: i64, b: i64) -> i64 {\n    a + b\n}\n",
    );
    write(d, "README.md", "# demo\n\nA tiny crate.\n");
    std::fs::remove_file(d.join("old.txt")).expect("rm");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "Widen add to i64"]);
    dir
}

#[test]
fn version_flag_prints_package_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dffy(dir.path(), &["--version"]);
    assert!(out.status.success());
    assert_eq!(
        stdout(&out),
        format!("dffy {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn print_mode_renders_a_range() {
    let repo = repo();
    let out = dffy(repo.path(), &["--color=never", "main~1..main"]);
    assert!(out.status.success(), "{}", stderr(&out));
    insta::assert_snapshot!(stdout(&out));
}

/// `--syntax` marks code that only moved with `»` and notes files it can't
/// diff by syntax; the config key does the same.
#[test]
fn syntax_flag_marks_moved_code_and_notes_unknown_languages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    let add = "int add(int a, int b)\n{\n\treturn a + b;\n}\n";
    let sub = "int sub(int a, int b)\n{\n\treturn a - b;\n}\n";
    write(d, "math.c", &format!("{add}\n{sub}"));
    write(d, "notes.txt", "one\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    write(d, "math.c", &format!("{sub}\n{add}"));
    write(d, "notes.txt", "two\n");
    let out = dffy(d, &["--color=never", "--unified", "--syntax"]);
    assert!(out.status.success(), "{}", stderr(&out));
    insta::assert_snapshot!(stdout(&out), @r"
    modified: math.c
    ────────────────
    @@ -1,9 +1,9 @@
    1   │»int add(int a, int b)
    2   │»{
    3   │»    return a + b;
    4   │»}
    5   │»
    6 1 │ int sub(int a, int b)
    7 2 │ {
    8 3 │     return a - b;
      4 │»}
      5 │»
      6 │»int add(int a, int b)
      7 │»{
      8 │»    return a + b;
    9 9 │ }

    modified: notes.txt
    ───────────────────
    no syntax diff: unknown language
    @@ -1 +1 @@
    1   │-one
      1 │+two
    ");
    let config = tempfile::tempdir().expect("tempdir");
    write(config.path(), "dffy/config", "syntax-diff = true\n");
    let configured = dffy_cmd(d, &["--color=never", "--unified"])
        .env("XDG_CONFIG_HOME", config.path())
        .output()
        .expect("spawn dffy");
    assert_eq!(stdout(&configured), stdout(&out));
    let line = dffy(d, &["--color=never", "--unified", "--no-syntax"]);
    assert!(!stdout(&line).contains(['»', '~']), "{}", stdout(&line));
}

/// A commit with one file for each kind of change `--json` describes:
/// an edit, an addition, a rename, a binary file, two functions swapped,
/// spacing changed and nothing else, and text that isn't UTF-8.
fn json_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    let bytes = |path: &str, content: &[u8]| std::fs::write(d.join(path), content).expect("write");
    git(d, &["init", "-q", "-b", "main"]);
    write(
        d,
        "lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    );
    write(d, "docs/guide.md", "# Guide\n\nRead me.\n");
    bytes("logo.bin", b"\x00\x01\x02");
    let add = "int add(int a, int b)\n{\n\treturn a + b;\n}\n";
    let sub = "int sub(int a, int b)\n{\n\treturn a - b;\n}\n";
    write(d, "math.c", &format!("{add}\n{sub}"));
    write(d, "spacing.c", "int x = 1;\nint y = 2;\nint z = 3;\n");
    bytes("latin1.txt", b"price \xa31\nthe end\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    write(
        d,
        "lib.rs",
        "pub fn add(a: i64, b: i64) -> i64 {\n    a + b\n}\n",
    );
    std::fs::create_dir_all(d.join("docs/user")).expect("mkdir");
    std::fs::rename(d.join("docs/guide.md"), d.join("docs/user/guide.md")).expect("mv");
    write(d, "new.txt", "brand new\nno newline");
    bytes("logo.bin", b"\x00\x01\x03");
    write(d, "math.c", &format!("{sub}\n{add}"));
    write(d, "spacing.c", "int x = 1;\nint  y=2;\nint z = 3;\n");
    bytes("latin1.txt", b"price \xa32\nthe end\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "change"]);
    dir
}

/// `--json` for the range limited to `paths`, with `args` before them.
fn json(d: &Path, args: &[&str], paths: &[&str]) -> String {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    full.extend_from_slice(&["main~1", "main", "--"]);
    full.extend_from_slice(paths);
    let out = dffy(d, &full);
    assert!(out.status.success(), "{full:?}: {}", stderr(&out));
    stdout(&out)
}

/// A paired edit carries its emphasis; an addition, a rename and a binary
/// file each have their own fields; a paired line that isn't UTF-8 is lossy
/// and has no emphasis.
#[test]
fn json_describes_each_kind_of_change() {
    let repo = json_repo();
    let d = repo.path();
    insta::assert_snapshot!("json_modified", json(d, &[], &["lib.rs"]));
    insta::assert_snapshot!("json_added", json(d, &[], &["new.txt"]));
    insta::assert_snapshot!("json_renamed", json(d, &[], &["docs"]));
    insta::assert_snapshot!("json_binary", json(d, &[], &["logo.bin"]));
    insta::assert_snapshot!("json_not_utf8", json(d, &[], &["latin1.txt"]));
}

/// `--syntax` marks swapped functions moved, and the file `moved_only`;
/// spacing changes are `formatting_only` by the syntax diff's marks, and
/// by `-w`, which leaves the file no hunks.
#[test]
fn json_classifies_moved_and_formatting_only_files() {
    let repo = json_repo();
    let d = repo.path();
    insta::assert_snapshot!("json_moved", json(d, &["--syntax"], &["math.c"]));
    insta::assert_snapshot!(
        "json_formatting",
        [
            json(d, &["--syntax"], &["spacing.c"]),
            json(d, &["-w"], &["spacing.c"]),
        ]
        .join("--\n")
    );
    // A file only -w finds unchanged is no difference for --exit-code.
    let exit = |args: &[&str]| {
        let mut full = vec!["--json", "--exit-code"];
        full.extend_from_slice(args);
        full.extend_from_slice(&["main~1", "main", "--", "spacing.c"]);
        dffy(d, &full).status.code()
    };
    assert_eq!(exit(&[]), Some(1));
    assert_eq!(exit(&["-w"]), Some(0));
}

/// With a summary flag, file objects have their counts, flags and language
/// but no hunks; `-w` still classifies whitespace-only files, and
/// `--syntax` adds what only the syntax diff can tell.
#[test]
fn json_summaries_leave_out_hunks() {
    let repo = json_repo();
    let d = repo.path();
    insta::assert_snapshot!(
        "json_summary",
        [
            json(d, &["--stat"], &[]),
            json(d, &["--name-status", "-w"], &["spacing.c"]),
            json(d, &["--name-only", "--syntax"], &["math.c"]),
        ]
        .join("--\n")
    );
    // Every summary flag gives the same objects, and the counts of the
    // full output.
    let summary = json(d, &["--stat"], &[]);
    assert_eq!(summary, json(d, &["--name-status"], &[]));
    let end = |out: &str| out.lines().last().map(str::to_owned);
    assert_eq!(end(&summary), end(&json(d, &[], &[])));
}

/// `--json` describes the repository; a diff on stdin isn't one.
#[test]
fn json_refuses_a_diff_on_stdin() {
    let repo = json_repo();
    let out = dffy(repo.path(), &["--json", "-"]);
    assert_eq!(out.status.code(), Some(128));
    assert!(stderr(&out).contains("--json"), "{}", stderr(&out));
}

#[test]
fn show_prints_the_commit_header_first() {
    let repo = repo();
    let out = dffy(repo.path(), &["--color=never", "show", "--name-status"]);
    assert!(out.status.success(), "{}", stderr(&out));
    insta::assert_snapshot!(stdout(&out));
}

#[test]
fn summaries_match_git_formats() {
    let repo = repo();
    let run = |args: &[&str]| stdout(&dffy(repo.path(), args));
    insta::assert_snapshot!(
        "summaries",
        [
            run(&["--stat", "main~1", "main"]),
            run(&["--name-only", "main~1", "main"]),
            run(&["--name-status", "main~1", "main"]),
            run(&["--name-status", "-R", "main~1", "main"]),
        ]
        .join("--\n")
    );
}

/// A commit that moves files around: an exact rename, an edited one, one
/// into a deeper directory, and a deletion with nothing similar added.
fn renames_repo() -> tempfile::TempDir {
    use std::fmt::Write as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    let body = |tag: &str, n: usize| -> String {
        (0..n).fold(String::new(), |mut out, i| {
            let _ = writeln!(out, "{tag} line {i}");
            out
        })
    };
    git(d, &["init", "-q", "-b", "main"]);
    write(d, "docs/guide.md", &body("guide", 20));
    write(d, "src/net/socket.c", &body("socket", 30));
    write(d, "src/util.c", &body("util", 30));
    write(d, "gone.txt", &body("gone", 10));
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    for path in [
        "docs/guide.md",
        "src/util.c",
        "src/net/socket.c",
        "gone.txt",
    ] {
        std::fs::remove_file(d.join(path)).expect("rm");
    }
    write(d, "docs/user/guide.md", &body("guide", 20));
    write(d, "src/helpers.c", &(body("util", 26) + "extra\n"));
    write(d, "src/net/core/sock.c", &body("socket", 30));
    write(d, "fresh.txt", "brand new\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "move"]);
    dir
}

#[test]
fn renames_print_like_git() {
    let repo = renames_repo();
    let d = repo.path();
    let ours = |args: &[&str]| {
        let mut full = vec!["--color=never"];
        full.extend_from_slice(args);
        stdout(&dffy(d, &full))
    };
    let theirs = |args: &[&str]| {
        let mut full = vec!["diff", "--no-ext-diff", "-M"];
        full.extend_from_slice(args);
        git(d, &full)
    };
    assert_eq!(
        ours(&["--name-status", "main~1", "main"]),
        theirs(&["--name-status", "main~1", "main"])
    );
    assert_eq!(
        ours(&["--name-only", "main~1", "main"]),
        theirs(&["--name-only", "main~1", "main"])
    );
    assert_eq!(
        ours(&["--stat", "main~1", "main"]),
        theirs(&["--stat=80", "main~1", "main"])
    );
    assert_eq!(
        ours(&["--name-status", "--no-renames", "main~1", "main"]),
        theirs(&["--name-status", "--no-renames", "main~1", "main"])
    );
    let diff = ours(&["main~1", "main"]);
    assert!(
        diff.contains("renamed: src/util.c → src/helpers.c (86% similar)"),
        "{diff}"
    );
}

/// Plain `dffy` lists everything not committed: `git diff HEAD` plus the
/// untracked files. `--unstaged` is plain `git diff`, `--staged` is
/// `git diff --cached`, and `default-diff = unstaged` makes plain `dffy`
/// the former.
#[test]
fn default_diff_is_everything_uncommitted() {
    let repo = repo();
    let d = repo.path();
    let ours = |args: &[&str]| {
        let mut full = vec!["--color=never", "--name-status", "--no-renames"];
        full.extend_from_slice(args);
        let out = dffy(d, &full);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        stdout(&out)
    };
    let theirs = |args: &[&str]| {
        let mut full = vec!["diff", "--no-ext-diff", "--name-status", "--no-renames"];
        full.extend_from_slice(args);
        git(d, &full)
    };
    write(d, "README.md", "# demo\n\nStaged.\n");
    write(d, "staged-new.txt", "new\n");
    git(d, &["add", "README.md", "staged-new.txt"]);
    write(d, "README.md", "# demo\n\nStaged, then edited.\n");
    write(d, "src/lib.rs", "pub fn nothing() {}\n");
    write(d, "untracked.txt", "u\n");

    let all = format!("{}?\tuntracked.txt\n", theirs(&["HEAD"]));
    assert_eq!(ours(&[]), all);
    assert_eq!(
        all,
        "M\tREADME.md\nM\tsrc/lib.rs\nA\tstaged-new.txt\n?\tuntracked.txt\n"
    );
    assert_eq!(ours(&["--unstaged"]), theirs(&[]));
    assert_eq!(ours(&["--staged"]), theirs(&["--cached"]));
    assert_eq!(ours(&["HEAD"]), theirs(&["HEAD"]));

    let config = tempfile::tempdir().expect("tempdir");
    write(config.path(), "dffy/config", "default-diff = unstaged\n");
    let configured = |args: &[&str]| {
        let out = dffy_cmd(d, args)
            .env("XDG_CONFIG_HOME", config.path())
            .output()
            .expect("spawn dffy");
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        stdout(&out)
    };
    let names = ["--color=never", "--name-status", "--no-renames"];
    assert_eq!(configured(&names), theirs(&[]));
    assert_eq!(
        configured(&[&names[..], &["--staged"]].concat()),
        theirs(&["--cached"])
    );
    assert_eq!(
        configured(&[&names[..], &["HEAD"]].concat()),
        theirs(&["HEAD"])
    );
    // `--all` is the default spelled out, for one run over the config.
    assert_eq!(configured(&[&names[..], &["--all"]].concat()), all);
    assert_eq!(ours(&["--all"]), all);
    for clash in [&["--all", "--staged"][..], &["--all", "HEAD"]] {
        assert_eq!(dffy(d, clash).status.code(), Some(128), "{clash:?}");
    }

    // Staging everything leaves plain git diff empty; dffy still lists it.
    git(d, &["add", "-A"]);
    assert_eq!(theirs(&[]), "");
    assert_eq!(
        ours(&[]),
        "M\tREADME.md\nM\tsrc/lib.rs\nA\tstaged-new.txt\nA\tuntracked.txt\n"
    );
}

/// Before the first commit everything, staged or not, is an addition.
#[test]
fn default_diff_works_before_the_first_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    write(d, "a.txt", "a\n");
    write(d, "dir/b.txt", "b\n");
    git(d, &["add", "-A"]);
    write(d, "c.txt", "c\n");
    let run = |args: &[&str]| stdout(&dffy(d, args));
    assert_eq!(
        run(&["--name-status"]),
        "A\ta.txt\nA\tdir/b.txt\n?\tc.txt\n"
    );
    assert_eq!(
        run(&["--name-status", "--staged"]),
        git(d, &["diff", "--no-ext-diff", "--name-status", "--cached"])
    );
    assert_eq!(run(&["--name-status", "--unstaged"]), "");
    let out = dffy(d, &["--color=never", "--unstaged", "HEAD"]);
    assert_eq!(out.status.code(), Some(128), "{}", stderr(&out));
}

#[test]
fn colored_output_is_syntax_highlighted_unless_turned_off() {
    let repo = repo();
    let run = |args: &[&str]| stdout(&dffy(repo.path(), args));
    let keyword = "\x1b[0;35mpub";
    let on = run(&["--color=always", "main~1..main"]);
    assert!(on.contains(keyword), "{on:?}");
    // The changed type is emphasized whole, in the added color.
    assert!(on.contains("\x1b[0;7;32mi64"), "{on:?}");
    let off = run(&["--color=always", "--no-highlight", "main~1..main"]);
    assert!(!off.contains(keyword), "{off:?}");
    assert_eq!(
        run(&["--color=never", "main~1..main"]),
        run(&["--color=never", "--no-highlight", "main~1..main"])
    );
}

/// Truecolor terminals get background tints, others bars, and plain output
/// always gets `+`/`-` signs whatever the config says.
#[test]
fn change_indicators_follow_the_terminal_and_fall_back_to_signs_without_color() {
    let repo = repo();
    let run = |args: &[&str], colorterm: Option<&str>, config: Option<&str>| {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cmd = dffy_cmd(repo.path(), args);
        if let Some(text) = config {
            write(dir.path(), "dffy/config", text);
            cmd.env("XDG_CONFIG_HOME", dir.path());
        }
        if let Some(value) = colorterm {
            cmd.env("COLORTERM", value);
        }
        stdout(&cmd.output().expect("spawn dffy"))
    };
    let range = ["--color=always", "main~1..main"];
    let tinted = run(&range, Some("truecolor"), None);
    assert!(tinted.contains(";48;2;"), "{tinted:?}");
    assert!(!tinted.contains('▌'), "{tinted:?}");
    let bars = run(&range, None, None);
    assert!(bars.contains('▌') && !bars.contains(";48;2;"), "{bars:?}");
    let none = run(&range, Some("truecolor"), Some("indicators = none\n"));
    assert!(
        !none.contains('▌') && !none.contains("│\x1b[0;32m+"),
        "{none:?}"
    );
    for config in [None, Some("indicators = background\n")] {
        let plain = run(
            &["--color=never", "main~1..main"],
            Some("truecolor"),
            config,
        );
        assert!(plain.contains("│+A tiny crate."), "{plain}");
        assert!(plain.contains("│-remove me"), "{plain}");
    }
}

#[test]
fn stat_colors_the_graph_when_asked() {
    let repo = repo();
    let out = dffy(repo.path(), &["--color=always", "--stat", "main~1", "main"]);
    let text = stdout(&out);
    assert!(text.contains("\x1b[0;32m+"), "{text:?}");
    assert!(text.contains("\x1b[0;31m-"), "{text:?}");
}

#[test]
fn pathspecs_are_relative_to_the_current_directory() {
    let repo = repo();
    let out = dffy(
        &repo.path().join("src"),
        &["--name-only", "main~1", "main", "--", "lib.rs"],
    );
    assert_eq!(stdout(&out), "src/lib.rs\n", "{}", stderr(&out));
    let out = dffy(
        &repo.path().join("src"),
        &["--name-only", "main~1", "main", "--", ".."],
    );
    assert_eq!(stdout(&out), "README.md\nold.txt\nsrc/lib.rs\n");
}

#[test]
fn exit_code_reports_differences() {
    let repo = repo();
    assert_eq!(dffy(repo.path(), &["--exit-code"]).status.code(), Some(0));
    assert_eq!(
        dffy(repo.path(), &["--exit-code", "main", "main"])
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        dffy(repo.path(), &["--exit-code", "main~1", "main"])
            .status
            .code(),
        Some(1)
    );
    write(repo.path(), "README.md", "changed\n");
    let out = dffy(repo.path(), &["--exit-code", "--name-only"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), "README.md\n");
}

#[test]
fn unknown_revisions_suggest_the_closest_ref() {
    let repo = repo();
    let out = dffy(repo.path(), &["--color=never", "amin"]);
    assert_eq!(out.status.code(), Some(128));
    assert_eq!(
        stderr(&out),
        "error: unknown revision 'amin'\nhint: did you mean 'main'?\n"
    );
    assert_eq!(stdout(&out), "");
}

/// An argument echoed in an error reaches the terminal escaped, so it
/// can't carry escape sequences or reorder the line.
#[test]
fn errors_escape_the_arguments_they_echo() {
    let repo = repo();
    let out = dffy(repo.path(), &["--color=never", "x\x1b[2J\u{202e}y"]);
    assert_eq!(out.status.code(), Some(128));
    let err = stderr(&out);
    assert!(
        !err.contains('\x1b') && !err.contains('\u{202e}'),
        "{err:?}"
    );
    assert!(err.contains("x^[[2J<U+202E>y"), "{err:?}");
}

#[test]
fn outside_a_repository_mentions_dash_c() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dffy(dir.path(), &["--color=never", "HEAD"]);
    assert_eq!(out.status.code(), Some(128));
    assert!(stderr(&out).contains("-C <path>"), "{}", stderr(&out));
}

#[test]
fn usage_errors_exit_128() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dffy(dir.path(), &["--no-such-flag"]);
    assert_eq!(out.status.code(), Some(128));
    assert_eq!(dffy(dir.path(), &["--help"]).status.code(), Some(0));
}

#[test]
fn piped_diff_renders_in_pager_mode_anywhere() {
    let dir = tempfile::tempdir().expect("tempdir");
    for args in [&["--color=always"][..], &["--color=always", "-"]] {
        let mut child = dffy_cmd(dir.path(), args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn dffy");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(FIXTURE)
            .expect("write diff");
        let out = child.wait_with_output().expect("dffy exits");
        assert!(out.status.success(), "{}", stderr(&out));
        let text = stdout(&out);
        assert!(
            text.contains("\x1b[0;1;34mmodified: calc.c\x1b[0m"),
            "{text}"
        );
        // An edited line: its new token green, the rest of it in the
        // modified color.
        assert!(text.contains("\x1b[0;7;32mlong\x1b[0;34m"), "{text}");
    }
}

/// Printed with color, an edited line and its old version are in the
/// modified color with their changed tokens red and green; the config's
/// `modified-lines = added-removed` gives red and green lines, and a choice
/// remembered by the viewer doesn't reach printed output.
#[test]
fn printed_edits_show_as_modified() {
    let repo = repo();
    let print = || {
        stdout(&dffy(
            repo.path(),
            &["--color=always", "HEAD^1", "--", "src/lib.rs"],
        ))
    };
    write(
        repo.path(),
        ".git/dffy-test-state/dffy/state",
        "version = 1\nmodified-lines = added-removed\n",
    );
    let config = stdout(&dffy(repo.path(), &["--color=never", "config"]));
    assert!(
        config.contains("modified-lines = added-removed  # last used"),
        "the viewer would take it: {config}"
    );
    // Highlighted: the code keeps its syntax colors, the bars and numbers
    // say modified.
    let text = print();
    assert_eq!(text.matches("\x1b[0;34m▌").count(), 2, "{text:?}");
    assert!(text.contains("\x1b[0;7;31mi32"), "{text:?}");
    assert!(text.contains("\x1b[0;7;32mi64"), "{text:?}");
    write(
        repo.path(),
        ".no-config/dffy/config",
        "modified-lines = added-removed\n",
    );
    let text = print();
    assert!(!text.contains("\x1b[0;34m▌"), "{text:?}");
    assert!(
        text.contains("\x1b[0;31m▌") && text.contains("\x1b[0;32m▌"),
        "{text:?}"
    );
}

/// As git runs `interactive.diffFilter`: stdin and stdout are pipes, and
/// the output is colored line for line, with no pager. Under `NO_COLOR`
/// the input's own colors are stripped and none are added.
#[test]
fn color_only_colors_a_pipe_line_for_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = include_bytes!("fixtures/git-diff-color.diff");
    let run = |no_color: bool| {
        let mut cmd = dffy_cmd(dir.path(), &["--color-only"]);
        if no_color {
            cmd.env("NO_COLOR", "1");
        }
        let mut child = cmd
            .env("DFFY_PAGER", "false")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn dffy");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(input)
            .expect("write diff");
        let out = child.wait_with_output().expect("dffy exits");
        assert!(out.status.success(), "{}", stderr(&out));
        out.stdout
    };
    let lines = |b: &[u8]| b.split(|&c| c == b'\n').count();
    let colored = run(false);
    assert_eq!(lines(&colored), lines(input));
    let text = String::from_utf8_lossy(&colored);
    assert!(
        text.contains("\x1b[0;1;34mdiff --git a/calc.c b/calc.c\x1b[m\n"),
        "{text}"
    );
    let plain = run(true);
    assert_eq!(lines(&plain), lines(input));
    assert!(!plain.contains(&0x1b));
    let out = dffy(dir.path(), &["--color-only", "HEAD"]);
    assert_eq!(out.status.code(), Some(128));
}

#[test]
fn closed_stdout_exits_quietly() {
    let mut child = dffy_cmd(Path::new("/"), &["-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dffy");
    drop(child.stdout.take());
    let mut stdin = child.stdin.take().expect("piped stdin");
    for _ in 0..200 {
        if stdin.write_all(FIXTURE).is_err() {
            break;
        }
    }
    drop(stdin);
    let out = child.wait_with_output().expect("dffy exits");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stderr(&out), "");
}

#[test]
fn config_file_values_flags_and_warnings() {
    let repo = repo();
    write(
        repo.path(),
        ".no-config/dffy/config",
        "tab-width = 8\ncolour = always\n",
    );
    let out = dffy(repo.path(), &["--color=never", "config", "--theme=mono"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("tab-width = 8  # config\n"), "{text}");
    assert!(text.contains("theme = mono  # flag\n"), "{text}");
    assert!(text.contains("color = never  # flag\n"), "{text}");
    assert!(text.contains("wrap = false  # default\n"), "{text}");
    let warning = stderr(&out);
    assert!(
        warning.contains("config:2: unknown key 'colour'"),
        "{warning}"
    );
    assert!(warning.contains("hint: did you mean 'color'?"), "{warning}");
}

/// What the viewer remembered beats the config file and loses to a flag;
/// `remember = false` ignores it; a file that isn't dffy's state counts
/// as nothing.
#[test]
fn remembered_view_choices_sit_between_flags_and_the_config() {
    let repo = repo();
    let config_dir = repo.path().join(".no-config/dffy");
    std::fs::create_dir_all(&config_dir).expect("mkdir");
    std::fs::write(config_dir.join("config"), "view = split\nwrap = true\n").expect("write");
    let state = repo.path().join(".git/dffy-test-state/dffy");
    std::fs::create_dir_all(&state).expect("mkdir");
    std::fs::write(
        state.join("state"),
        "version = 1\nview = unified\nline-numbers = false\n",
    )
    .expect("write");
    let config = |args: &[&str]| {
        let mut all = vec!["--color=never", "config"];
        all.extend(args);
        stdout(&dffy(repo.path(), &all))
    };
    let text = config(&[]);
    assert!(text.contains("view = unified  # last used\n"), "{text}");
    assert!(
        text.contains("line-numbers = false  # last used\n"),
        "{text}"
    );
    assert!(text.contains("wrap = true  # config\n"), "{text}");
    std::fs::write(
        config_dir.join("config"),
        "view = split\nremember = false\n",
    )
    .expect("write");
    let text = config(&[]);
    assert!(text.contains("view = split  # config\n"), "{text}");
    assert!(text.contains("line-numbers = true  # default\n"), "{text}");
    std::fs::write(config_dir.join("config"), "").expect("write");
    std::fs::write(state.join("state"), "\u{0}garbage = = =\n").expect("write");
    let out = dffy(repo.path(), &["--color=never", "config"]);
    assert!(out.status.success());
    assert_eq!(
        stderr(&out),
        "",
        "a corrupt state file isn't the user's to fix"
    );
    assert!(stdout(&out).contains("view = auto  # default\n"));
}

/// Whole files, ignored whitespace and watching, remembered by the
/// viewer, leave printed output as it was: no full text, the whitespace
/// change still shown, no warning that watching needs the viewer.
#[test]
fn remembered_viewer_modes_leave_printed_output_alone() {
    let repo = repo();
    let d = repo.path();
    let lines: Vec<String> = (1..=40).map(|i| format!("let x{i} = {i};\n")).collect();
    write(d, "long.rs", &lines.concat());
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "Add long.rs"]);
    let mut changed = lines;
    "let x5 = 500;\n".clone_into(&mut changed[4]);
    "let  x30  =  30;\n".clone_into(&mut changed[29]);
    write(d, "long.rs", &changed.concat());
    let print = || dffy(d, &["--color=never", "--", "long.rs"]);
    let before = print();
    assert!(stdout(&before).contains("x30"), "{}", stdout(&before));
    write(
        d,
        ".git/dffy-test-state/dffy/state",
        "version = 1\ncontext = full\nignore-all-space = true\nwatch = true\n",
    );
    let after = print();
    assert_eq!(stdout(&after), stdout(&before));
    assert_eq!(stderr(&after), "");
    let config = stdout(&dffy(d, &["--color=never", "config"]));
    for line in [
        "context = full  # last used\n",
        "ignore-all-space = true  # last used\n",
        "watch = true  # last used\n",
    ] {
        assert!(config.contains(line), "{line}{config}");
    }
}

#[test]
fn themes_lists_builtins_and_user_files() {
    let repo = repo();
    let themes = repo.path().join(".no-config/dffy/themes");
    std::fs::create_dir_all(&themes).expect("mkdir");
    std::fs::write(themes.join("mine"), "added = #00ff00\n").expect("write theme");
    let text = stdout(&dffy(repo.path(), &["--color=always", "themes"]));
    for name in [
        "default",
        "mono",
        "github-dark",
        "dracula",
        "solarized-light",
        "mine",
    ] {
        assert!(text.contains(name), "{name} missing:\n{text}");
    }
    // Without COLORTERM the sample has bars, in the theme's added color.
    assert!(text.contains("\x1b[0;38;2;0;255;0m▌"), "{text:?}");
    let plain = stdout(&dffy(repo.path(), &["--color=never", "themes"]));
    let groups: Vec<&str> = plain
        .lines()
        .filter(|l| !l.starts_with([' ', '*']) && !l.is_empty())
        .collect();
    assert_eq!(groups, ["Terminal palette", "Dark", "Light"], "{plain}");
    assert!(
        plain.contains("  13 -│     let sum = count + 1;"),
        "{plain}"
    );
}

fn unstyled(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            chars.by_ref().find(|&c| c == 'm');
        } else {
            out.push(c);
        }
    }
    out
}

/// The copy's sample in `dffy themes` is byte-identical to the built-in's.
#[test]
fn a_dumped_theme_loads_back_as_the_same_theme() {
    let repo = repo();
    let dumped = dffy(repo.path(), &["themes", "--dump", "dracula"]);
    assert!(dumped.status.success(), "{}", stderr(&dumped));
    let text = stdout(&dumped);
    assert!(text.contains("\nbackground = #282a36\n"), "{text}");
    write(repo.path(), ".no-config/dffy/themes/copy", &text);
    let out = dffy_cmd(repo.path(), &["--color=always", "themes"])
        .env("COLORTERM", "truecolor")
        .output()
        .expect("spawn dffy");
    assert_eq!(stderr(&out), "");
    let listing = stdout(&out);
    let lines: Vec<&str> = listing.lines().collect();
    let sample = |name: &str| {
        let at = lines
            .iter()
            .position(|l| unstyled(l).trim_start().starts_with(&format!("{name}  ")))
            .unwrap_or_else(|| panic!("{name} missing:\n{listing}"));
        lines[at + 1..at + 5].join("\n")
    };
    assert!(sample("dracula").contains(";48;2;"), "{listing:?}");
    assert_eq!(sample("copy"), sample("dracula"));

    let unknown = dffy(repo.path(), &["themes", "--dump", "dracul"]);
    assert_eq!(unknown.status.code(), Some(128));
    assert!(
        stderr(&unknown).contains("no built-in theme 'dracul'; did you mean 'dracula'?"),
        "{}",
        stderr(&unknown)
    );
}

#[test]
fn theme_files_inherit_and_mistakes_warn_with_a_suggestion() {
    let repo = repo();
    write(
        repo.path(),
        ".no-config/dffy/themes/mine",
        "inherit = dracula\nadded = #00ff00\nadde = red\n",
    );
    let out = dffy(
        repo.path(),
        &["--color=always", "--theme", "mine", "themes"],
    );
    let err = stderr(&out);
    assert!(err.contains("themes/mine:3: unknown key 'adde'"), "{err}");
    assert!(err.contains("did you mean 'added'?"), "{err}");
    let listing = unstyled(&stdout(&out));
    let dark = listing.split("\nLight\n").next().expect("text");
    assert!(
        dark.contains("* mine  "),
        "mine is dark, as dracula is:\n{listing}"
    );
    assert!(
        stdout(&out).contains("\x1b[0;38;2;0;255;0;48;2;40;42;54m▌"),
        "{out:?}"
    );

    let out = dffy(
        repo.path(),
        &["--color=never", "--theme", "dracla", "themes"],
    );
    assert!(
        stderr(&out).contains("unknown theme 'dracla'; did you mean 'dracula'?"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn doctor_reports_each_check() {
    let repo = repo();
    let out = dffy_cmd(repo.path(), &["--color=never", "doctor"])
        .env("TERM", "xterm-256color")
        .env_remove("TMUX")
        .env_remove("ZELLIJ")
        .env_remove("STY")
        .output()
        .expect("spawn dffy");
    let text = stdout(&out);
    let labels: Vec<String> = text
        .lines()
        .map(|l| {
            l.chars()
                .skip(2)
                .take(12)
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .filter(|l| !l.starts_with("hint"))
        .collect();
    assert_eq!(
        labels,
        [
            "git",
            "repository",
            "watch",
            "speed",
            "terminal",
            "multiplexer",
            "color",
            "config",
            "theme",
            "pager",
            "cache",
            "dffyd"
        ],
        "{text}"
    );
    assert!(text.contains("TERM=xterm-256color"), "{text}");
    let repository = text.lines().nth(1).unwrap_or_default();
    let path = repo.path().canonicalize().expect("canonical");
    assert!(
        repository.contains(&format!("{} · ", path.display())),
        "{repository}"
    );
    assert!(text.contains(" · all "), "{text}");
}

/// `--json` is one object, with the measurement as numbers; outside a
/// repository the repository check says so and nothing is measured.
#[test]
fn doctor_json_and_outside_a_repository() {
    let repo = repo();
    let json = stdout(&dffy(repo.path(), &["doctor", "--json"]));
    assert!(
        json.starts_with("{\"build\":\"") && json.ends_with("}\n"),
        "{json}"
    );
    assert!(!json.contains('\x1b'), "no color: {json}");
    let changes = json
        .split("\"changes\":")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .and_then(|n| n.parse::<usize>().ok());
    assert!(changes.is_some(), "{json}");
    assert!(json.contains("\"repo_ms\":"), "{json}");

    let outside = tempfile::tempdir().expect("tempdir");
    let dir = outside.path().to_str().expect("utf-8 path");
    let out = dffy(repo.path(), &["--color=never", "-C", dir, "doctor"]);
    let text = stdout(&out);
    assert!(
        text.contains("· repository  not in a git repository (-C <path>)"),
        "{text}"
    );
    assert!(!text.contains(" speed "), "{text}");
    assert!(out.status.success(), "{out:?}");
}

/// The review plugin `--review` names is `dffy-review-<name>` in an
/// absolute PATH directory: a renderer of that name isn't it, and neither
/// is one in a relative directory.
#[test]
fn doctor_finds_the_review_plugin_on_path() {
    use std::os::unix::fs::PermissionsExt as _;
    let repo = repo();
    let bin = tempfile::tempdir().expect("tempdir");
    let renderers = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(repo.path().join("rel")).expect("mkdir");
    for file in [
        bin.path().join("dffy-review-test"),
        renderers.path().join("dffy-render-test"),
        repo.path().join("rel/dffy-review-test"),
    ] {
        std::fs::write(&file, "#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let doctor = |path: String, json: bool| {
        let args: &[&str] = if json {
            &["--review", "test", "doctor", "--json"]
        } else {
            &["--color=never", "--review", "test", "doctor"]
        };
        stdout(
            &dffy_cmd(repo.path(), args)
                .env("PATH", path)
                .output()
                .expect("spawn dffy"),
        )
    };
    let system = "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin";
    let found = doctor(format!("rel:{}:{system}", bin.path().display()), true);
    let plugin = bin.path().join("dffy-review-test");
    assert!(
        found.contains(&format!(
            "\"review\":{{\"name\":\"test\",\"path\":\"{}\",",
            plugin.display()
        )),
        "{found}"
    );
    let missing = doctor(format!("rel:{}:{system}", renderers.path().display()), true);
    assert!(
        missing.contains("\"review\":{\"name\":\"test\",\"path\":null,"),
        "{missing}"
    );
    let text = doctor(format!("{}:{system}", renderers.path().display()), false);
    assert!(
        text.contains("! review      dffy-review-test isn't on PATH"),
        "{text}"
    );
}

/// The test review plugin as `dffy-review-test` in its own directory:
/// `describe` answers `answer` and adds a line to `described`, so a test
/// counts the runs; `doctor` reports one check.
fn describing_plugin(answer: &str) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = tempfile::tempdir().expect("tempdir");
    let plugin = bin.path().join("dffy-review-test");
    std::fs::write(&plugin, include_str!("fixtures/stubs/review-plugin.sh")).expect("write");
    std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::write(bin.path().join("describe.out"), answer).expect("write");
    std::fs::write(
        bin.path().join("doctor.out"),
        concat!(
            r#"{"version":1,"checks":[{"name":"thing","ok":false,"detail":"not set up"}]}"#,
            "\n"
        ),
    )
    .expect("write");
    bin
}

const MERGE_REQUEST: &str =
    r#"{"version":1,"command":"mr","noun":"merge request","nouns":"merge requests"}"#;

/// The review command is the one the plugin describes, in help and in
/// completions: `dffy mr` reaches the review code (which then wants a
/// terminal), and `pr` is a revision like any other.
#[test]
fn the_review_command_is_the_one_the_plugin_describes() {
    let repo = repo();
    let bin = describing_plugin(MERGE_REQUEST);
    let path = format!("{}:/usr/bin:/bin", bin.path().display());
    let run = |args: &[&str]| {
        let mut full = vec!["--review", "test"];
        full.extend(args);
        dffy_cmd(repo.path(), &full)
            .env("PATH", &path)
            .output()
            .expect("spawn dffy")
    };
    let help = stdout(&run(&["--help"]));
    assert!(
        help.contains("mr           Review a merge request in the viewer"),
        "{help}"
    );
    assert!(!help.contains("  pr "), "{help}");
    let mr = run(&["mr", "7"]);
    assert_eq!(mr.status.code(), Some(128));
    assert!(
        stderr(&mr).contains("dffy mr opens the viewer, which needs a terminal"),
        "{}",
        stderr(&mr)
    );
    let pr = run(&["pr"]);
    assert_eq!(pr.status.code(), Some(128));
    assert!(stderr(&pr).contains("'pr'"), "{}", stderr(&pr));
    let zsh = stdout(&run(&["completions", "zsh"]));
    assert!(zsh.contains("'mr:Review a merge request"), "{zsh}");
    let doctor = stdout(&run(&["--color=never", "doctor"]));
    assert!(doctor.contains(", adds dffy mr\n"), "{doctor}");
    assert!(doctor.contains("! review thing not set up"), "{doctor}");
}

/// `describe` runs once per installed version of the program.
#[test]
fn describe_is_cached_until_the_plugin_changes() {
    let repo = repo();
    let bin = describing_plugin(MERGE_REQUEST);
    let path = format!("{}:/usr/bin:/bin", bin.path().display());
    let runs = || {
        std::fs::read_to_string(bin.path().join("described"))
            .unwrap_or_default()
            .lines()
            .count()
    };
    let help = || {
        let out = dffy_cmd(repo.path(), &["--review", "test", "--help"])
            .env("PATH", &path)
            .output()
            .expect("spawn dffy");
        assert!(stdout(&out).contains("Review a merge request"), "{out:?}");
    };
    help();
    help();
    assert_eq!(runs(), 1);
    let plugin = bin.path().join("dffy-review-test");
    let mut script = std::fs::read_to_string(&plugin).expect("read");
    script.push_str("# changed\n");
    std::fs::write(&plugin, script).expect("write");
    help();
    assert_eq!(runs(), 2);
}

/// Without a review plugin, or with one whose command is dffy's own,
/// there's no review command: `pr` is a revision, help lists none, and
/// doctor says why.
#[test]
fn without_a_described_plugin_there_is_no_review_command() {
    let repo = repo();
    let pr = dffy(repo.path(), &["pr", "7"]);
    assert_eq!(pr.status.code(), Some(128));
    assert!(stderr(&pr).contains("'pr'"), "{}", stderr(&pr));
    let help = stdout(&dffy(repo.path(), &["--help"]));
    assert!(!help.contains("Review a"), "{help}");

    let bin = describing_plugin(&MERGE_REQUEST.replace("\"mr\"", "\"diff\""));
    let path = format!("{}:/usr/bin:/bin", bin.path().display());
    let run = |args: &[&str]| {
        dffy_cmd(repo.path(), args)
            .env("PATH", &path)
            .output()
            .expect("spawn dffy")
    };
    let help = stdout(&run(&["--review", "test", "--help"]));
    assert!(!help.contains("Review a"), "{help}");
    let diff = run(&[
        "--review",
        "test",
        "--color=never",
        "main~1..main",
        "--stat",
    ]);
    assert!(diff.status.success(), "{}", stderr(&diff));
    let doctor = stdout(&run(&["--review", "test", "--color=never", "doctor"]));
    assert!(doctor.contains(", adds no command\n"), "{doctor}");
    assert!(
        doctor.contains("its command 'diff' is one of dffy's own"),
        "{doctor}"
    );
}

/// The plugin is asked to describe itself only by a run that could use
/// its command: not by a range, `--stat`, a diff on stdin or dffy's own
/// subcommands, even with the cache off.
#[test]
fn describe_runs_only_where_the_review_command_could_be() {
    let repo = repo();
    let bin = describing_plugin(MERGE_REQUEST);
    let path = format!("{}:/usr/bin:/bin", bin.path().display());
    let run = |args: &[&str], stdin: &[u8]| {
        let mut full = vec!["--review", "test", "--color=never"];
        full.extend(args);
        full.push("--no-cache");
        let mut child = dffy_cmd(repo.path(), &full)
            .env("PATH", &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn dffy");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(stdin)
            .expect("write stdin");
        let out = child.wait_with_output().expect("dffy exits");
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
    };
    let runs = || {
        std::fs::read_to_string(bin.path().join("described"))
            .unwrap_or_default()
            .lines()
            .count()
    };
    run(&["main~1..main"], b"");
    run(&["diff", "--stat"], b"");
    run(&["--stat"], b"");
    run(&[], FIXTURE);
    run(&["config"], b"");
    run(&["comments", "apply"], b"[]");
    assert_eq!(runs(), 0);
    run(&["--help"], b"");
    assert_eq!(runs(), 1);
}

/// When the plugin can't describe itself, its command is a revision, and
/// the error for it says why the command is missing.
#[test]
fn a_plugin_that_cant_describe_itself_is_named_where_its_command_fails() {
    let repo = repo();
    let bin = describing_plugin("not an answer\n");
    let path = format!("{}:/usr/bin:/bin", bin.path().display());
    let out = dffy_cmd(
        repo.path(),
        &["--review", "test", "--color=never", "mr", "7"],
    )
    .env("PATH", &path)
    .output()
    .expect("spawn dffy");
    assert_eq!(out.status.code(), Some(128));
    let said = stderr(&out);
    assert!(said.starts_with("error: unknown revision 'mr'\n"), "{said}");
    assert!(
        said.contains("hint: dffy-review-test adds no command: "),
        "{said}"
    );
    let quiet = dffy_cmd(
        repo.path(),
        &["--review", "test", "--color=never", "main~1..main"],
    )
    .env("PATH", &path)
    .output()
    .expect("spawn dffy");
    assert!(quiet.status.success(), "{}", stderr(&quiet));
    assert_eq!(stderr(&quiet), "", "no warning on every start");
}

#[test]
fn completions_and_man_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let zsh = stdout(&dffy(dir.path(), &["completions", "zsh"]));
    assert!(zsh.starts_with("#compdef dffy"), "{zsh}");
    let man = stdout(&dffy(dir.path(), &["man"]));
    assert!(man.contains(".TH dffy"), "{man}");
}

/// `task docs` writes docs/keys.md and docs/config.md from this output, and
/// CI fails when they differ: it must not depend on the machine's config,
/// remembered state or color settings.
#[test]
fn docs_markdown_ignores_the_users_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    let run = |page| {
        let out = dffy(d, &["config", "--markdown", page]);
        assert!(out.status.success(), "{}", stderr(&out));
        stdout(&out)
    };
    let clean = [run("keys"), run("config")];
    write(
        d,
        ".no-config/dffy/config",
        "view = split\nkeybind = n=next_file\nkeybind = leader:s=unbind\n",
    );
    write(
        d,
        ".git/dffy-test-state/dffy/state",
        "version = 1\nwrap = true\n",
    );
    let configured = [run("keys"), run("config")];
    let forced = ["keys", "config"].map(|page| {
        let out = dffy_cmd(d, &["config", "--markdown", page])
            .env("CLICOLOR_FORCE", "1")
            .output()
            .expect("spawn dffy");
        stdout(&out)
    });
    assert_eq!(clean, configured);
    assert_eq!(clean, forced);
    for page in &clean {
        assert!(
            page.starts_with("<!-- generated by task docs: do not edit -->\n"),
            "{page}"
        );
    }
    assert!(
        clean[0].contains("| `next_change` | `n` `}` |"),
        "{}",
        clean[0]
    );
}

#[test]
fn daemon_commands_without_a_broker() {
    let dir = tempfile::tempdir().expect("tempdir");
    for action in ["status", "stop"] {
        let out = dffy(dir.path(), &["--color=never", "daemon", action]);
        assert!(out.status.success(), "{action}");
        assert!(stdout(&out).contains("dffyd isn't running"), "{action}");
    }
}

/// `git add -p` runs `interactive.diffFilter` with both ends piped, so dffy
/// prints instead of opening the viewer. git requires one output line per
/// diff line, which dffy's rendering breaks with separators and rewritten
/// file headers, so git refuses it; `--color-only` is the mode for this.
#[test]
fn as_interactive_diff_filter_dffy_prints_and_git_refuses_the_line_count() {
    let dir = repo();
    let d = dir.path();
    write(d, "README.md", "# demo\n\nMore.\n");
    let filter = format!("{} --paging=never", env!("CARGO_BIN_EXE_dffy"));
    let mut child = Command::new("git")
        .current_dir(d)
        .args(["-c", &format!("interactive.diffFilter={filter}")])
        // git filters the colored diff only, so color is forced on.
        .args(["-c", "color.ui=always", "add", "-p"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("XDG_CONFIG_HOME", d.join(".no-config"))
        .env("XDG_STATE_HOME", d.join(".git/dffy-test-state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git runs");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"q\n")
        .expect("answer");
    let out = child.wait_with_output().expect("git exits");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("mismatched output from interactive.diffFilter"),
        "{}",
        stderr(&out)
    );
}

/// dffy with `args`, `comments apply` among them, and `ops` on stdin.
fn comments_apply(dir: &Path, args: &[&str], ops: &str) -> Output {
    let mut child = dffy_cmd(dir, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dffy");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(ops.as_bytes())
        .expect("write operations");
    child.wait_with_output().expect("dffy exits")
}

/// The ids of an `apply` answer, `{"ids":["…",…]}`, none of them null.
fn answer_ids(out: &Output) -> Vec<String> {
    assert!(out.status.success(), "{}", stderr(out));
    let answer = stdout(out);
    let list = answer
        .strip_prefix(r#"{"ids":["#)
        .and_then(|s| s.strip_suffix("]}\n"));
    assert!(list.is_some(), "an answer: {answer}");
    list.expect("checked")
        .split(',')
        .map(|id| id.trim_matches('"').to_owned())
        .collect()
}

/// The write contract end to end: operations go in through `apply`, the
/// file comes out of `--json` exactly as written, a failing batch leaves
/// the file as it was, and the listing shows the threads.
#[test]
fn comments_apply_round_trips_through_the_default_file() {
    let repo = repo();
    let d = repo.path();
    let anchor = r#"{"before":[],"lines":["pub fn add(a: i64, b: i64) -> i64 {"],"after":["    a + b","}"]}"#;
    let apply = ["comments", "apply"];
    let added = comments_apply(
        d,
        &apply,
        &format!(
            r#"[{{"op":"add","comment":{{"path":"src/lib.rs","start":{{"old":null,"new":1}},"end":{{"old":null,"new":1}},"text":"Why i64?","created":"2026-11-17T10:42:00Z","anchor":{anchor},"github_thread":7}}}},
            {{"op":"add","comment":{{"path":"README.md","text":"Say more.","author":"octocat","created":"2026-11-17T10:43:00Z"}}}}]"#
        ),
    );
    let ids = answer_ids(&added);
    assert_eq!(ids.len(), 2);
    let replied = comments_apply(
        d,
        &apply,
        &format!(
            r#"[{{"op":"add","comment":{{"in_reply_to":"{}","text":"It overflowed.\nTwice.","author":"octocat","created":"2026-11-17T10:44:00Z"}}}},{{"op":"set","id":"{}","fields":{{"resolved":true}}}}]"#,
            ids[0], ids[0]
        ),
    );
    let reply = &answer_ids(&replied)[0];
    let file = d.join(".git/dffy/comments.json");
    let written = std::fs::read_to_string(&file).expect("apply wrote the default file");
    let json = dffy(d, &["comments", "--json"]);
    assert!(json.status.success(), "{}", stderr(&json));
    assert_eq!(stdout(&json), written);
    assert!(
        written.starts_with(r#"{"version":1,"comments":["#),
        "{written}"
    );
    assert!(
        written.contains(&format!(r#""in_reply_to":"{}""#, ids[0])),
        "{written}"
    );
    assert!(written.contains(r#""resolved":true"#), "{written}");
    assert!(written.contains(r#""github_thread":7"#), "{written}");
    // The local file's threads go on the branch they were added on.
    let branch = String::from_utf8(
        Command::new("git")
            .current_dir(d)
            .args(["branch", "--show-current"])
            .output()
            .expect("git")
            .stdout,
    )
    .expect("utf-8");
    assert_eq!(
        written
            .matches(&format!(r#""branch":"{}""#, branch.trim()))
            .count(),
        3,
        "{written}"
    );

    let failed = comments_apply(
        d,
        &apply,
        r#"[{"op":"notice","notice":"lost"},{"op":"set","id":"nope","fields":{"text":"x"}}]"#,
    );
    assert_eq!(failed.status.code(), Some(128));
    assert_eq!(
        stderr(&failed),
        "error: operation 1 (set): comment nope doesn't exist\n"
    );
    assert_eq!(stdout(&failed), "");
    assert_eq!(
        std::fs::read_to_string(&file).expect("still there"),
        written
    );

    let listed = stdout(&dffy(d, &["--color=never", "comments"]))
        .replace(&ids[0], "<root>")
        .replace(&ids[1], "<file>")
        .replace(reply.as_str(), "<reply>");
    insta::assert_snapshot!(listed, @r"
    README.md
      <file> octocat, 2026-11-17T10:43:00Z
        Say more.

    src/lib.rs:1, resolved
      <root> you, 2026-11-17T10:42:00Z
        Why i64?
      <reply> octocat, 2026-11-17T10:44:00Z
        It overflowed.
        Twice.
    ");
}

/// `--comments` picks the file before the subcommand, between it and
/// `apply`, or after, relative to `-C`; without it, outside a repository,
/// there's no file to use.
#[test]
fn comments_takes_its_file_from_the_flag_anywhere() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    std::fs::create_dir(d.join("pr")).expect("mkdir");
    let ops = r#"[{"op":"add","comment":{"path":"a.txt","text":"Hi."}},{"op":"notice","notice":"stale"}]"#;
    for (args, file) in [
        (
            &["-C", "pr", "--comments", "one.json", "comments", "apply"][..],
            "pr/one.json",
        ),
        (&["comments", "--comments", "two.json", "apply"], "two.json"),
        (
            &["comments", "apply", "--comments", "three.json"],
            "three.json",
        ),
    ] {
        let out = comments_apply(d, args, ops);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        assert!(stdout(&out).ends_with(",null]}\n"), "{}", stdout(&out));
        assert!(d.join(file).is_file(), "{args:?}");
    }
    let read = dffy(d, &["comments", "--comments", "pr/one.json", "--json"]);
    assert!(read.status.success(), "{}", stderr(&read));
    assert!(
        stdout(&read).starts_with(r#"{"version":1,"notice":"stale","comments":["#),
        "{}",
        stdout(&read)
    );
    assert!(!stdout(&read).contains("\"branch\""), "{}", stdout(&read));
    let listed = dffy(
        d,
        &["--color=never", "--comments", "pr/one.json", "comments"],
    );
    assert!(
        stdout(&listed).starts_with("notice: stale\n"),
        "{}",
        stdout(&listed)
    );

    let none = dffy(d, &["comments", "--comments", "missing.json", "--json"]);
    assert_eq!(stdout(&none), "{\"version\":1,\"comments\":[\n]}\n");
    let out = dffy(d, &["comments", "--json"]);
    assert_eq!(out.status.code(), Some(128));
    assert!(
        stderr(&out).contains("--comments <file>"),
        "{}",
        stderr(&out)
    );
}

/// The local file holds every branch's comments: `dffy comments` lists the
/// current branch's threads and those on none, `--branch` another's and
/// `--all-branches` all, each by state and file too; `rm --resolved`
/// removes the resolved threads it would list, replies and all.
#[test]
fn comments_are_listed_and_removed_by_branch_state_and_path() {
    let repo = repo();
    let d = repo.path();
    write(
        d,
        ".git/dffy/comments.json",
        r#"{"version":1,"comments":[
{"id":"m1","path":"src/lib.rs","text":"Open on main.","author":"","created":"t1","resolved":false,"branch":"main"},
{"id":"m2","path":"README.md","text":"Done on main.","author":"","created":"t2","resolved":true,"branch":"main"},
{"id":"m3","path":"README.md","text":"Reply.","author":"ada","created":"t3","in_reply_to":"m2","resolved":false,"branch":"main"},
{"id":"u1","path":"src/lib.rs","text":"On no branch.","author":"","created":"t4","resolved":true},
{"id":"t1","path":"src/lib.rs","text":"On topic.","author":"","created":"t5","resolved":true,"branch":"topic"}
]}"#,
    );
    let ids = |args: &[&str]| {
        let mut all = vec!["--color=never", "comments"];
        all.extend(args);
        let out = dffy(d, &all);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        let text = stdout(&out);
        let mut ids: Vec<String> = text
            .lines()
            .filter_map(|l| {
                let id = l.strip_prefix("  ")?.split(' ').next()?;
                (!id.is_empty()).then(|| id.to_owned())
            })
            .collect();
        ids.sort();
        (ids, text)
    };
    let (here, text) = ids(&[]);
    assert_eq!(here, ["m1", "m2", "m3", "u1"]);
    assert!(
        text.ends_with("\n1 more on other branches; --all-branches lists them\n"),
        "{text}"
    );
    assert_eq!(ids(&["--open"]).0, ["m1"]);
    assert_eq!(ids(&["--resolved", "--path", "README.md"]).0, ["m2", "m3"]);
    assert_eq!(ids(&["--branch", "topic"]).0, ["t1", "u1"]);
    let (all, text) = ids(&["--all-branches", "--resolved"]);
    assert_eq!(all, ["m2", "m3", "t1", "u1"]);
    assert!(
        text.contains("src/lib.rs, branch topic, resolved\n"),
        "{text}"
    );
    let markdown = dffy(d, &["comments", "--markdown", "--open"]);
    assert_eq!(
        stdout(&markdown),
        "## `src/lib.rs`\n\n**you**, t1\n\nOpen on main.\n"
    );

    let removed = dffy(d, &["comments", "rm", "--resolved"]);
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert_eq!(stdout(&removed), "removed 3 comments\n");
    assert_eq!(ids(&["--all-branches"]).0, ["m1", "t1"]);
    let removed = dffy(d, &["comments", "rm", "--resolved", "--branch", "topic"]);
    assert_eq!(stdout(&removed), "removed 1 comment\n");
    assert_eq!(ids(&["--all-branches"]).0, ["m1"]);

    for (args, error) in [
        (&["comments", "rm"][..], "--resolved"),
        (&["comments", "--open", "rm", "--resolved"], "for listing"),
        (&["comments", "--json", "--open"], "cannot be used with"),
    ] {
        let out = dffy(d, args);
        assert!(!out.status.success(), "{args:?}");
        assert!(stderr(&out).contains(error), "{args:?}: {}", stderr(&out));
    }
}

/// `--session` reads the comments file of the review session the viewer
/// has open, as the session pointer in the shared git directory names it,
/// and says so when there's none.
#[test]
fn comments_session_reads_the_file_the_pointer_names() {
    let repo = repo();
    let d = repo.path();
    let out = dffy(d, &["comments", "--session"]);
    assert_eq!(out.status.code(), Some(128));
    assert!(
        stderr(&out).contains("no review session"),
        "{}",
        stderr(&out)
    );
    let pr = d.join(".git/review/7.json");
    write(
        d,
        ".git/review/7.json",
        r#"{"version":1,"summary":"Two nits.","comments":[{"id":"p1","path":"src/lib.rs","text":"Posted.","author":"octocat","created":"t1","resolved":false,"ref":"github:1"}]}"#,
    );
    write(
        d,
        ".git/dffy/session",
        &format!(
            r#"{{"version":1,"comments":"{}","review":"7","branch":"main","head":"abc1234"}}"#,
            pr.display()
        ),
    );
    let listed = dffy(d, &["--color=never", "comments", "--session"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    insta::assert_snapshot!(stdout(&listed), @r"
    summary, not posted yet
        Two nits.

    src/lib.rs
      p1 octocat, t1, posted
        Posted.
    ");
    let both = dffy(d, &["--comments", "x.json", "comments", "--session"]);
    assert!(stderr(&both).contains("give one"), "{}", stderr(&both));
}

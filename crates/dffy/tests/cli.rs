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

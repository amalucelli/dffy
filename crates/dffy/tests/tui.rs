//! The TUI end to end in a pseudo-terminal: the built binary runs on a
//! fixture repository while the test types keys, resizes the terminal and
//! reads the screen back through a terminal emulator.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::io::{Read, Write};
use std::num::NonZero;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Term {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    output: Arc<Mutex<Vec<u8>>>,
    /// Ends once the child closed the terminal and all output is in.
    reading: std::thread::JoinHandle<()>,
    vt: vt100::Parser,
    seen: usize,
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl Term {
    fn spawn(dir: &Path, cols: u16, rows: u16, env: &[(&str, &str)]) -> Self {
        Self::spawn_command(
            CommandBuilder::new(env!("CARGO_BIN_EXE_dffy")),
            dir,
            cols,
            rows,
            env,
        )
    }

    /// Runs `script` with `sh` in the terminal, `$DFFY` naming the binary:
    /// for pipelines into dffy.
    fn spawn_sh(dir: &Path, cols: u16, rows: u16, script: &str) -> Self {
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", script]);
        cmd.env("DFFY", env!("CARGO_BIN_EXE_dffy"));
        Self::spawn_command(cmd, dir, cols, rows, &[])
    }

    fn spawn_command(
        mut cmd: CommandBuilder,
        dir: &Path,
        cols: u16,
        rows: u16,
        env: &[(&str, &str)],
    ) -> Self {
        let pair = native_pty_system()
            .openpty(size(cols, rows))
            .expect("open a pty");
        cmd.cwd(dir);
        cmd.env("TERM", "xterm-256color");
        cmd.env("XDG_CONFIG_HOME", dir.join(".no-config"));
        // Inside the git directory, so the cache never shows as a change.
        cmd.env("XDG_CACHE_HOME", dir.join(".git/dffy-test-cache"));
        // Never the user's own; the first-run tip already shown, so the
        // status line reads as it does from then on.
        let state = if dir.join(".git").is_dir() {
            dir.join(".git/dffy-test-state")
        } else {
            dir.join(".dffy-test-state")
        };
        std::fs::create_dir_all(state.join("dffy")).expect("state dir");
        if !state.join("dffy/state").exists() {
            std::fs::write(state.join("dffy/state"), "version = 1\ntip-shown = true\n")
                .expect("state file");
        }
        cmd.env("XDG_STATE_HOME", &state);
        for name in [
            "NO_COLOR",
            "CLICOLOR_FORCE",
            "COLORTERM",
            "DFFY_PAGER",
            "COLUMNS",
            "VISUAL",
            "TMUX",
        ] {
            cmd.env_remove(name);
        }
        for (name, value) in env {
            cmd.env(name, value);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn dffy");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        let reading = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().expect("lock").extend_from_slice(&buf[..n]);
            }
        });
        let writer = pair.master.take_writer().expect("pty writer");
        let rows = NonZero::new(rows).expect("rows");
        let cols = NonZero::new(cols).expect("cols");
        Self {
            master: pair.master,
            writer,
            child,
            output,
            reading,
            vt: vt100::Parser::new(rows, cols, 0),
            seen: 0,
        }
    }

    fn output(&self) -> Vec<u8> {
        self.output.lock().expect("lock").clone()
    }

    /// Feeds new output to the emulator; returns whether there was any.
    fn update(&mut self) -> bool {
        let out = self.output();
        let new = out.len() > self.seen;
        self.vt.process(&out[self.seen..]);
        self.seen = out.len();
        new
    }

    fn row(&self, y: u16) -> String {
        let (_, cols) = self.vt.screen().size();
        self.vt
            .screen()
            .contents_between(y, 0, y, cols.get())
            .trim_end()
            .to_owned()
    }

    /// The bottom row: the status bar, in the frame's border or alone.
    fn status(&self) -> String {
        let (rows, _) = self.vt.screen().size();
        self.row(rows.get() - 1)
    }

    fn wait_for(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let start = Instant::now();
        loop {
            self.update();
            if done(self) {
                return;
            }
            assert!(
                start.elapsed() < TIMEOUT,
                "timed out waiting for {what}; screen:\n{}",
                self.vt.screen().contents()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits until output stops for a moment, so the last key's frame is in.
    fn settle(&mut self) {
        let mut quiet = Instant::now();
        let start = Instant::now();
        while quiet.elapsed() < Duration::from_millis(150) && start.elapsed() < TIMEOUT {
            if self.update() {
                quiet = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).expect("send keys");
        self.writer.flush().expect("flush keys");
        self.settle();
    }

    fn contains(&self, text: &str) -> bool {
        self.vt.screen().contents().contains(text)
    }

    /// Waits for the child to exit and for the last of its output.
    fn wait_exit(&mut self) -> portable_pty::ExitStatus {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("wait") {
                break status;
            }
            assert!(start.elapsed() < TIMEOUT, "dffy didn't exit");
            std::thread::sleep(Duration::from_millis(10));
        };
        while !self.reading.is_finished() && start.elapsed() < TIMEOUT {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.update();
        status
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada Lovelace")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?} failed");
}

/// A config directory holding `text` as dffy's config.
fn config(text: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("dffy")).expect("mkdir");
    std::fs::write(dir.path().join("dffy/config"), text).expect("write");
    dir
}

fn path(dir: &tempfile::TempDir) -> String {
    dir.path().to_str().expect("utf-8 path").to_owned()
}

/// A repository with uncommitted changes: README.md with one hunk, then
/// src/lib.rs with three, far enough apart that a 12-row terminal scrolls.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    let lines: Vec<String> = (1..=200).map(|i| format!("line {i}\n")).collect();
    std::fs::create_dir(d.join("src")).expect("mkdir");
    std::fs::write(d.join("src/lib.rs"), lines.concat()).expect("write");
    std::fs::write(d.join("README.md"), "# demo\n").expect("write");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    let mut changed = lines;
    for (i, text) in [
        (2, "line three\n"),
        (99, "line one hundred\n"),
        (179, "line one hundred eighty\n"),
    ] {
        text.clone_into(&mut changed[i]);
    }
    std::fs::write(d.join("src/lib.rs"), changed.concat()).expect("write");
    std::fs::write(d.join("README.md"), "# demo\n\nMore.\n").expect("write");
    dir
}

#[test]
fn keys_resize_and_quit_restore_the_terminal() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 100, 12, &[]);
    term.wait_for("the first frame", |t| t.contains("README.md"));
    term.wait_for("the diffs", |t| t.contains("More.") && t.contains(" +3 -3"));
    assert!(term.vt.screen().alternate_screen());
    assert!(term.vt.screen().hide_cursor());
    // 12 rows: no frame, the title on the first row, the status on the
    // last.
    assert!(
        term.status()
            .ends_with("/ search · tab tree · space commands"),
        "{}",
        term.status()
    );
    assert!(term.status().contains(" 1/2"), "{}", term.status());
    assert!(term.row(0).contains("README.md · +2 -0"), "{}", term.row(0));
    // 100 columns: the file tree is a panel beside the diff.
    assert!(term.row(1).starts_with("┌ Files · 2"), "{}", term.row(1));

    // `n` puts the next change at the top, as far as the file allows;
    // past the file's last, it opens the next file as it opens the first
    // time: here its first change is on the first screen, so from the top.
    term.send("n");
    assert!(
        term.contains("3 ▌ More."),
        "{}",
        term.vt.screen().contents()
    );
    term.send("n");
    assert!(term.contains("▌ line 3"), "{}", term.vt.screen().contents());
    assert!(term.status().contains(" 2/2 "), "{}", term.status());
    assert!(
        term.row(0).contains("src/lib.rs · +3 -3"),
        "{}",
        term.row(0)
    );

    term.send("T");
    assert!(term.row(1).starts_with("@@ -1,"), "{}", term.row(1));

    // Once the note about the file `n` went to is gone.
    status_shows(&mut term, "unified");
    term.send("s");
    assert!(term.status().contains("split"), "{}", term.status());

    term.send("?");
    assert!(term.contains(" keybinds "), "help shows");
    term.send("q");
    assert!(!term.contains(" keybinds "), "help closes");

    // The kernel signals the size change; dffy redraws at the new size,
    // narrow now, so without the frame: the file switcher is the first row
    // and the narrow hint ends the last.
    term.vt.screen_mut().set_size(
        NonZero::new(24).expect("rows"),
        NonZero::new(80).expect("cols"),
    );
    let resized_at = term.output().len();
    term.master.resize(size(80, 24)).expect("resize");
    term.wait_for("a redraw at 80 columns", |t| {
        let (_, cols) = t.vt.screen().size();
        cols.get() == 80
            && t.row(0).starts_with(" ‹ 2/2 lib.rs › ")
            && t.status().ends_with("␣ commands")
            && t.output()[resized_at..].windows(4).any(|w| w == b"\x1b[2J")
    });

    term.send("q");
    let status = term.wait_exit();
    assert!(status.success(), "{status}");
    let out = term.output();
    let tail = &out[resized_at..];
    for sequence in [&b"\x1b[?1049l"[..], b"\x1b[?25h", b"\x1b[?1000l"] {
        assert!(
            tail.windows(sequence.len()).any(|w| w == sequence),
            "missing {sequence:?} on exit"
        );
    }
    assert!(!term.vt.screen().alternate_screen());
    assert!(!term.vt.screen().hide_cursor());
}

/// `?` opens the keys on the Move tab; `tab`, `→`, `2` and a tap on a title
/// switch tabs, and `esc` closes the overlay without leaving the viewer.
#[test]
fn the_keys_overlay_switches_tabs() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 100, 24, &[]);
    term.wait_for("the first frame", |t| t.contains("README.md"));
    term.send("?");
    assert!(term.contains("half page down"), "Move");
    term.send("\t");
    assert!(term.contains("in the tree"), "Files");
    term.send("\x1b[C");
    assert!(term.contains("toggle wrap"), "View");
    assert!(!term.contains("in the tree"));
    term.send("2");
    assert!(term.contains("in the tree"), "Files again");
    let y = (0..24)
        .find(|&y| term.row(y).contains(" Move  Files  View "))
        .expect("the tab titles");
    let row = term.row(y);
    let x = row[..row.find(" Copy ").expect("Copy")].chars().count() + 2;
    term.send(&format!("\x1b[<0;{x};{}M", y + 1));
    assert!(term.contains("copy selection"), "Copy, tapped");
    term.send("\x1b");
    assert!(!term.contains(" keybinds "), "closes");
    assert!(term.contains("README.md"));
    term.send("q");
    assert!(term.wait_exit().success());
}

#[test]
fn editor_gets_the_terminal_and_the_line() {
    let repo = repo();
    let bin = tempfile::tempdir().expect("tempdir");
    let record = bin.path().join("args");
    // Named like vim so it gets vim's `+line` argument.
    let editor = bin.path().join("vim");
    std::fs::write(
        &editor,
        format!(
            "#!/bin/sh\n[ -t 0 ] && tty=tty || tty=notty\necho \"$tty $*\" > '{}'\n",
            record.display()
        ),
    )
    .expect("write editor");
    Command::new("chmod")
        .arg("+x")
        .arg(&editor)
        .status()
        .expect("chmod");
    let editor = editor.to_str().expect("utf-8 path").to_owned();
    let mut term = Term::spawn(repo.path(), 100, 12, &[("EDITOR", &editor)]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    // The second change of src/lib.rs is at line 100.
    term.send("nnn");
    assert!(term.row(1).contains("▌ line 100"), "{}", term.row(1));
    term.send("e");
    term.wait_for("the editor to run", |_| record.exists());
    let args = std::fs::read_to_string(&record).expect("read args");
    let expected = repo
        .path()
        .canonicalize()
        .expect("canonical")
        .join("src/lib.rs");
    assert_eq!(args.trim(), format!("tty +100 {}", expected.display()));
    // Back in the viewer, keys work again.
    term.wait_for("the redraw", |t| t.row(1).contains("▌ line 100"));
    term.send("N");
    assert!(term.row(1).contains("▌ line 3"), "{}", term.row(1));
    term.send("q");
    assert!(term.wait_exit().success());
}

/// `c` opens a text box under the line; what's typed and pasted there is
/// saved to the repository's comments file on `esc`, and the thread shows
/// in its place with the line's mark.
#[test]
fn a_comment_is_written_in_a_box_and_saved() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 100, 12, &[]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    term.send("c");
    term.wait_for("the box", |t| t.contains("new comment"));
    term.send("Looks off");
    term.send("\x1b[200~ q,\r\nreally\x1b[201~");
    assert!(term.contains("really▏"), "{}", term.vt.screen().contents());
    term.send("\x1b");
    // Outside a review session nothing is posted, so it isn't a draft.
    term.wait_for("the thread", |t| t.contains("▾ you · just now ━"));
    assert!(term.contains("●"));
    let saved = std::fs::read_to_string(repo.path().join(".git/dffy/comments.json"))
        .expect("the comments file");
    assert!(
        saved.contains("\"text\":\"Looks off q,\\nreally\""),
        "{saved}"
    );
    term.send("q");
    assert!(term.wait_exit().success());
}

/// `ctrl+g` hands the box's text to the editor, on a file of its own, and
/// saves what's written there.
#[test]
fn ctrl_g_continues_a_comment_in_the_editor() {
    let repo = repo();
    let bin = tempfile::tempdir().expect("tempdir");
    let editor = bin.path().join("vim");
    std::fs::write(&editor, "#!/bin/sh\nprintf ' in vim\\n' >> \"$2\"\n").expect("write editor");
    Command::new("chmod")
        .arg("+x")
        .arg(&editor)
        .status()
        .expect("chmod");
    let editor = editor.to_str().expect("utf-8 path").to_owned();
    let mut term = Term::spawn(repo.path(), 100, 12, &[("EDITOR", &editor)]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    term.send("c");
    term.send("Begun");
    term.send("\x07");
    term.wait_for("the mark", |t| t.contains("●"));
    let saved = std::fs::read_to_string(repo.path().join(".git/dffy/comments.json"))
        .expect("the comments file");
    assert!(saved.contains("\"text\":\"Begun in vim\""), "{saved}");
    term.send("q");
    assert!(term.wait_exit().success());
}

/// A trip to the editor keeps the place: the diff stays where it was
/// scrolled, a failed editor opens the box again with the text and the
/// cursor at its end, and the saved thread takes the focus.
#[test]
fn the_editor_round_trip_keeps_the_place() {
    let repo = repo();
    let bin = tempfile::tempdir().expect("tempdir");
    let editor = bin.path().join("vim");
    let ran = bin.path().join("ran");
    std::fs::write(
        &editor,
        format!(
            "#!/bin/sh\n[ -e '{0}' ] || {{ touch '{0}'; exit 1; }}\nprintf ' in vim\\n' >> \"$2\"\n",
            ran.display()
        ),
    )
    .expect("write editor");
    Command::new("chmod")
        .arg("+x")
        .arg(&editor)
        .status()
        .expect("chmod");
    let editor = editor.to_str().expect("utf-8 path").to_owned();
    let mut term = Term::spawn(repo.path(), 100, 12, &[("EDITOR", &editor)]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    term.send("nnn");
    let top = term.row(1);
    assert!(top.contains("▌ line 100"), "{top}");
    term.send("c");
    term.wait_for("the box", |t| t.contains("new comment"));
    term.send("Begun\x07");
    term.wait_for("the box again", |t| ran.exists() && t.contains("Begun▏"));
    term.send("X");
    term.wait_for("the cursor at the end", |t| t.contains("BegunX▏"));
    assert_eq!(term.row(1), top);
    term.send("\x07");
    // A focused thread has a heavy border.
    term.wait_for("the focused thread", |t| {
        t.contains("BegunX in vim") && t.contains("┏")
    });
    // The same row, with the new thread's mark in its gutter.
    assert_eq!(
        term.row(1).replacen('●', " ", 1),
        top,
        "{}",
        term.vt.screen().contents()
    );
    term.send("q");
    assert!(term.wait_exit().success());
}

fn rev_parse(dir: &Path, rev: &str) -> String {
    String::from_utf8(git_out(dir, &["rev-parse", rev]))
        .expect("utf-8")
        .trim()
        .to_owned()
}

/// What `dffy-review-test` describes until a test writes another
/// `describe.sh`: `dffy pr`, a review read as `PR #7`, and four events.
const DESCRIBE: &str = r##"{"version":1,"command":"pr","noun":"pull request","nouns":"pull requests","short":"PR","id_prefix":"#","events":[{"id":"pending","key":"p","label":"pending","done":"review pending","submits":false},{"id":"comment","key":"c","label":"comment","done":"commented"},{"id":"approve","key":"a","label":"approve","done":"approved"},{"id":"request_changes","key":"r","label":"request changes","done":"changes requested"}]}"##;

/// A pull request for `dffy pr`: a repository whose last commit, the PR's
/// head, changes README.md over its base, the commit before; and
/// `dffy-review-test` on PATH, which records each call but `describe` and
/// how it was run, then runs `<verb>.sh` from its directory, which tests
/// write. It describes [`DESCRIBE`] until a test writes another
/// `describe.sh`, before its first run: the answer is cached.
struct Pull {
    repo: tempfile::TempDir,
    stub: tempfile::TempDir,
    base: String,
    head: String,
}

impl Pull {
    fn new() -> Self {
        let repo = tempfile::tempdir().expect("tempdir");
        let d = repo.path();
        git(d, &["init", "-q", "-b", "main"]);
        std::fs::write(d.join("README.md"), "# demo\n").expect("write");
        git(d, &["add", "-A"]);
        git(d, &["commit", "-q", "-m", "base"]);
        std::fs::write(d.join("README.md"), "# demo\n\nFrom the PR.\n").expect("write");
        git(d, &["commit", "-q", "-am", "head"]);
        let stub = tempfile::tempdir().expect("tempdir");
        let plugin = stub.path().join("dffy-review-test");
        std::fs::write(
            &plugin,
            "#!/bin/sh\n\
             d=$(dirname \"$0\")\n\
             [ \"$1\" = describe ] || \
             { echo \"$*\"; pwd -P; ( : < /dev/tty ) 2>/dev/null && echo tty || echo notty; \
               echo \"protocol=$DFFY_REVIEW_PROTOCOL comments=$DFFY_COMMENTS\"; } >> \"$d/calls\"\n\
             . \"$d/$1.sh\"\n",
        )
        .expect("write plugin");
        std::fs::write(
            stub.path().join("describe.sh"),
            format!("echo '{DESCRIBE}'"),
        )
        .expect("write describe");
        Command::new("chmod")
            .arg("+x")
            .arg(&plugin)
            .status()
            .expect("chmod");
        let base = rev_parse(d, "HEAD~1");
        let head = rev_parse(d, "HEAD");
        Self {
            repo,
            stub,
            base,
            head,
        }
    }

    fn comments(&self) -> std::path::PathBuf {
        self.stub.path().join("comments.json")
    }

    fn session(&self, number: u64, head: &str) -> String {
        format!(
            r#"{{"version":1,"session":{{"id":"{number}","title":"Spell out five","url":"https://example.com/pull/{number}","base":"{}","head":"{head}","last_reviewed_head":null,"comments":"{}"}}}}"#,
            self.base,
            self.comments().display()
        )
    }

    /// What `verb` does, as shell.
    fn verb(&self, verb: &str, script: &str) {
        std::fs::write(self.stub.path().join(format!("{verb}.sh")), script).expect("write");
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.stub.path().join("calls")).unwrap_or_default()
    }

    fn spawn(&self, args: &[&str]) -> Term {
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_dffy"));
        cmd.args(["--review", "test"]);
        cmd.args(args);
        let search = format!(
            "{}:{}",
            self.stub.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Term::spawn_command(cmd, self.repo.path(), 120, 16, &[("PATH", &search)])
    }
}

/// `dffy pr 7` says it's fetching until `prepare` names the PR's commits,
/// then opens its diff with its comments while the plugin goes on in the
/// background, its progress in the status bar. The plugin runs in the
/// working tree's root without a terminal, told the protocol.
#[test]
fn dffy_pr_opens_the_pr_once_prepare_names_its_commits() {
    let pull = Pull::new();
    std::fs::write(
        pull.comments(),
        r#"{"version":1,"comments":[{"id":"c1","path":"README.md","text":"Nice README","author":"octocat","ref":"github:1","created":"2026-11-17T10:00:00Z"}]}"#,
    )
    .expect("write");
    let gate = pull.stub.path().join("go");
    pull.verb(
        "prepare",
        &format!(
            "while [ ! -f '{}' ]; do sleep 0.02; done\necho '{}'\necho '{{\"version\":1,\"progress\":\"1/2\"}}'\nsleep 30\n",
            gate.display(),
            pull.session(7, &pull.head)
        ),
    );
    let mut term = pull.spawn(&["pr", "7"]);
    term.wait_for("the fetching note", |t| t.contains("fetching PR #7…"));
    std::fs::write(&gate, "").expect("open the gate");
    term.wait_for("the PR's diff", |t| t.contains("From the PR."));
    term.wait_for("its thread", |t| t.contains("Nice README"));
    term.wait_for("the progress", |t| {
        t.status().contains("PR #7 · fetching threads 1/2")
    });
    let calls = pull.calls();
    let root = pull.repo.path().canonicalize().expect("canonical");
    let lines: Vec<&str> = calls.lines().collect();
    assert_eq!(
        lines[..3],
        ["prepare 7", root.to_str().expect("utf-8"), "notty"]
    );
    assert_eq!(lines[3], "protocol=1 comments=");
    term.send("q");
    assert!(term.wait_exit().success());
}

/// A plugin that describes `mr` gets `dffy mr <id>`, in its words, and the
/// same review mode, its reviews named its way: `MR !7` for the review
/// whose id is `I8f2c` and label `7`. `pr` is then a revision. `describe`
/// runs without the terminal too.
#[test]
fn the_review_command_is_the_plugins() {
    let pull = Pull::new();
    pull.verb(
        "describe",
        r#"( : < /dev/tty ) 2>/dev/null && echo tty > "$d/describe-tty" || echo notty > "$d/describe-tty"
echo '{"version":1,"command":"mr","noun":"merge request","nouns":"merge requests","short":"MR","id_prefix":"!"}'"#,
    );
    let session = pull
        .session(7, &pull.head)
        .replace(r#""id":"7""#, r#""id":"I8f2c","label":"7""#);
    pull.verb("prepare", &format!("echo '{session}'"));
    let mut term = pull.spawn(&["mr", "I8f2c"]);
    term.wait_for("the merge request's diff", |t| t.contains("From the PR."));
    term.wait_for("review mode", |t| t.status().contains("MR !7"));
    assert!(
        pull.calls().starts_with("prepare I8f2c\n"),
        "{}",
        pull.calls()
    );
    assert_eq!(
        std::fs::read_to_string(pull.stub.path().join("describe-tty")).expect("described"),
        "notty\n"
    );
    term.send("q");
    assert!(term.wait_exit().success());
    let mut term = pull.spawn(&["pr", "7"]);
    let status = term.wait_exit();
    assert!(!status.success());
    let screen = term.vt.screen().contents();
    assert!(screen.contains("unknown revision 'pr'"), "{screen}");
}

/// Plain `dffy` on a branch asks the plugin for the branch's PR without
/// waiting for it, then keeps its own range with the PR's comments and
/// status; the command panel's "Show whole pull request" shows the PR's
/// base against the working tree, and goes back.
#[test]
fn plain_dffy_attaches_the_branchs_pr() {
    let pull = Pull::new();
    std::fs::write(
        pull.repo.path().join("README.md"),
        "# demo\n\nFrom the PR.\n\nLocal fix.\n",
    )
    .expect("write");
    std::fs::write(
        pull.comments(),
        r#"{"version":1,"comments":[{"id":"c1","path":"README.md","text":"Nice README","author":"octocat","ref":"github:1","created":"2026-11-17T10:00:00Z"}]}"#,
    )
    .expect("write");
    let gate = pull.stub.path().join("go");
    let session = pull.session(7, &pull.head);
    let pr = session.replacen("\"session\"", "\"review\"", 1);
    pull.verb(
        "detect",
        &format!(
            "while [ ! -f '{}' ]; do sleep 0.02; done\necho '{pr}'\n",
            gate.display()
        ),
    );
    pull.verb("prepare", &format!("echo '{session}'"));
    let mut term = pull.spawn(&[]);
    // Against HEAD: the two lines not committed.
    term.wait_for("the working tree's diff", |t| t.contains("+2 -0"));
    assert!(!term.status().contains("PR #7"), "{}", term.status());
    std::fs::write(&gate, "").expect("open the gate");
    term.wait_for("the PR", |t| t.status().contains("PR #7"));
    term.wait_for("its thread", |t| t.contains("Nice README"));
    assert!(term.contains("+2 -0"), "{}", term.vt.screen().contents());
    term.send(" A");
    // Against the PR's base: its commit's lines and the fix.
    term.wait_for("the PR's change and the fix", |t| t.contains("+4 -0"));
    assert!(
        term.contains("Local fix."),
        "{}",
        term.vt.screen().contents()
    );
    term.send(" A");
    term.wait_for("the working tree's diff again", |t| t.contains("+2 -0"));
    assert!(pull.calls().starts_with("detect\n"), "{}", pull.calls());
    term.send("q");
    assert!(term.wait_exit().success());
}

/// A review plugin that doesn't describe itself isn't asked for the
/// branch's review either.
#[test]
fn plain_dffy_asks_only_a_described_plugin_for_the_branchs_review() {
    let pull = Pull::new();
    pull.verb("describe", "exit 1");
    pull.verb("detect", r#"echo '{"version":1,"review":null}'"#);
    std::fs::write(pull.repo.path().join("README.md"), "# demo\n\nLocal.\n").expect("write");
    let mut term = pull.spawn(&[]);
    term.wait_for("the working tree's diff", |t| t.contains("Local."));
    std::thread::sleep(Duration::from_millis(300));
    term.send("q");
    assert!(term.wait_exit().success());
    assert_eq!(pull.calls(), "", "no verb ran");
}

/// What the plugin says went wrong before the PR's commits are here ends
/// `dffy pr` with it; a verb it doesn't know is worded.
#[test]
fn dffy_pr_says_why_a_pr_didnt_open() {
    let pull = Pull::new();
    for (script, said) in [
        (
            "echo '{\"version\":1,\"error\":\"no such PR\"}'; exit 1",
            "error: PR #7: no such PR",
        ),
        (
            "exit 2",
            "error: PR #7: dffy-review-test doesn't know prepare",
        ),
    ] {
        pull.verb("prepare", script);
        let mut term = pull.spawn(&["pr", "7"]);
        let status = term.wait_exit();
        assert!(!status.success());
        let screen = term.vt.screen().contents();
        assert!(screen.contains(said), "{screen}");
    }
}

/// A plugin still running when dffy quits is killed, with what it started.
#[test]
fn quitting_kills_the_plugin_running() {
    let pull = Pull::new();
    let pid = pull.stub.path().join("pid");
    pull.verb(
        "prepare",
        &format!(
            "echo '{}'\nsleep 30 & echo $! > '{}'\nwait\n",
            pull.session(7, &pull.head),
            pid.display()
        ),
    );
    let mut term = pull.spawn(&["pr", "7"]);
    term.wait_for("the PR's diff", |t| t.contains("From the PR."));
    term.wait_for("the sleep", |_| {
        std::fs::read_to_string(&pid).is_ok_and(|p| p.ends_with('\n'))
    });
    let pid = std::fs::read_to_string(&pid)
        .expect("pid")
        .trim()
        .to_owned();
    term.send("q");
    assert!(term.wait_exit().success());
    let start = Instant::now();
    while Command::new("kill")
        .args(["-0", &pid])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("kill")
        .success()
    {
        assert!(
            start.elapsed() < TIMEOUT,
            "the plugin's sleep outlived dffy"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Ctrl-C while `dffy pr` is still fetching kills the plugin, which runs
/// in a session of its own and so doesn't get the terminal's SIGINT.
#[test]
fn ctrl_c_while_fetching_kills_the_plugin() {
    let pull = Pull::new();
    let pid = pull.stub.path().join("pid");
    pull.verb(
        "prepare",
        &format!("sleep 30 & echo $! > '{}'\nwait\n", pid.display()),
    );
    let mut term = pull.spawn(&["pr", "7"]);
    term.wait_for("the fetching note", |t| t.contains("fetching PR #7…"));
    term.wait_for("the sleep", |_| {
        std::fs::read_to_string(&pid).is_ok_and(|p| p.ends_with('\n'))
    });
    let pid = std::fs::read_to_string(&pid)
        .expect("pid")
        .trim()
        .to_owned();
    term.send("\x03");
    assert_eq!(term.wait_exit().exit_code(), 130);
    let start = Instant::now();
    while Command::new("kill")
        .args(["-0", &pid])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("kill")
        .success()
    {
        if start.elapsed() > TIMEOUT {
            Command::new("kill").args(["-9", &pid]).status().ok();
            panic!("the plugin's sleep outlived dffy");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// "Since last review" from a head this clone doesn't have stays on the
/// whole PR and says why, each time it's asked.
#[test]
fn since_last_review_from_a_head_not_in_the_clone_stays_on_the_pr() {
    let pull = Pull::new();
    let session = pull.session(7, &pull.head).replace(
        r#""last_reviewed_head":null"#,
        r#""last_reviewed_head":"deadbeef""#,
    );
    pull.verb("prepare", &format!("echo '{session}'"));
    let mut term = pull.spawn(&["pr", "7"]);
    term.wait_for("the PR's diff", |t| t.contains("From the PR."));
    for _ in 0..2 {
        term.send(" ");
        term.send("R");
        term.wait_for("why not", |t| {
            t.status()
                .contains("last reviewed commit deadbee isn't in this clone")
        });
        assert!(
            !term.status().contains("since your review"),
            "{}",
            term.status()
        );
        assert!(term.contains("From the PR."));
    }
    term.send("q");
    assert!(term.wait_exit().success());
}

/// Without a number and with no PR on the branch, `dffy pr` opens on the
/// usual diff with the picker; picking one shows it in place. `p` then
/// pulls a new head, shown in place with the draft written before, and
/// `P` posts the review with the event and summary picked.
#[test]
fn a_picked_pr_updates_in_place_and_posts() {
    let pull = Pull::new();
    let d = pull.repo.path();
    let first = rev_parse(d, "HEAD");
    std::fs::write(d.join("README.md"), "# demo\n\nFrom the PR.\n\nAnd more.\n").expect("write");
    git(d, &["commit", "-q", "-am", "head 2"]);
    let second = rev_parse(d, "HEAD");
    git(d, &["reset", "-q", "--hard", &first]);
    // The second head stays reachable for the update.
    git(d, &["update-ref", "refs/pull/11/head", &second]);
    pull.verb(
        "list",
        r#"echo '{"version":1,"reviews":[{"id":"11","title":"Document the cache","author":"ada","url":"u","head_ref":"docs","reason":"mine"}]}'"#,
    );
    pull.verb("prepare", &format!("echo '{}'", pull.session(11, &first)));
    pull.verb("pull", &format!("echo '{}'", pull.session(11, &second)));
    let summary = pull.stub.path().join("summary");
    pull.verb(
        "post",
        &format!(
            "[ \"$3\" = --dry-run ] && exit 0\n\
             echo \"$*\" > '{}.args'; cat \"$6\" > '{}'\n\
             echo '{{\"version\":1,\"progress\":\"1/1\"}}'\n\
             echo '{{\"version\":1,\"posted\":1,\"failed\":0,\"url\":\"https://example.com/pull/11#review\"}}'\n",
            summary.display(),
            summary.display()
        ),
    );
    let mut term = pull.spawn(&["pr"]);
    term.wait_for("the picker", |t| t.contains("Yours") && t.contains("#11"));
    term.send("\r");
    term.wait_for("the PR", |t| t.status().contains("PR #11"));
    term.wait_for("its diff", |t| t.contains("From the PR."));

    term.send("c");
    term.send("Draft here");
    term.send("\x1b");
    term.wait_for("the draft", |t| t.contains("Draft here"));
    term.send("p");
    term.wait_for("the new head", |t| t.contains("And more."));
    term.wait_for("the count", |t| {
        t.status().contains("PR #11 updated · 1 new commit")
    });
    assert!(
        term.contains("Draft here"),
        "{}",
        term.vt.screen().contents()
    );

    term.send("P");
    term.wait_for("the dialog", |t| t.contains("post review · PR #11"));
    assert!(
        term.contains("1 comment"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("c");
    term.send("s");
    term.send("Looks good");
    term.send("\x1b");
    term.send("\r");
    term.wait_for("the post", |t| t.status().contains("review sent"));
    let args = std::fs::read_to_string(summary.with_extension("args")).expect("args");
    assert!(
        args.starts_with("post 11 --event comment --summary-file "),
        "{args}"
    );
    assert_eq!(
        std::fs::read_to_string(&summary).expect("summary"),
        "Looks good"
    );
    term.send("q");
    assert!(term.wait_exit().success());
}

/// `P` runs `post --dry-run` beside the `prepare` still going, and the
/// dialog counts what it says. A post the host refused a comment in asks
/// what to do; `s` applies the approval without the comments left out.
#[test]
fn posting_checks_first_and_asks_when_a_comment_is_refused() {
    let pull = Pull::new();
    std::fs::write(
        pull.comments(),
        r#"{"version":1,"comments":[{"id":"c1","path":"README.md","text":"Draft here","created":"2026-11-17T10:00:00Z"},{"id":"c2","path":"README.md","text":"And here","created":"2026-11-17T10:01:00Z"}]}"#,
    )
    .expect("write");
    pull.verb(
        "prepare",
        &format!("echo '{}'\nsleep 30\n", pull.session(7, &pull.head)),
    );
    let skipped = r#"{"version":1,"draft":{"id":"c1","path":"README.md","plan":"skip","reason":"file not in the PR"}}"#;
    let refused = r#"{"version":1,"draft":{"id":"c2","path":"README.md","plan":"skip","reason":"Line could not be resolved"}}"#;
    let args = pull.stub.path().join("submit.args");
    pull.verb(
        "post",
        &format!(
            "case \"$*\" in\n\
             *--dry-run*) echo '{skipped}'; echo '{{\"version\":1,\"draft\":{{\"id\":\"c2\",\"path\":\"README.md\",\"plan\":\"file\"}}}}'; \
               echo '{{\"version\":1,\"planned\":{{\"line\":0,\"file\":1,\"skip\":1}}}}' ;;\n\
             *--apply-event*) echo \"$*\" > '{}'; echo '{{\"version\":1,\"posted\":0,\"failed\":0,\"url\":\"https://example.com/pull/7#review\"}}' ;;\n\
             *) echo '{skipped}'; echo '{refused}'; echo '{{\"version\":1,\"posted\":0,\"failed\":2,\"held\":true,\"held_message\":\"review still pending\",\"url\":\"https://example.com/pull/7#review\"}}' ;;\n\
             esac\n",
            args.display()
        ),
    );
    let mut term = pull.spawn(&["pr", "7"]);
    term.wait_for("the PR's diff", |t| t.contains("From the PR."));
    term.send("P");
    term.wait_for("the counts", |t| {
        t.contains("2 comments · 1 as a file comment")
            && t.contains("1 can't be posted and stays a draft")
    });
    assert!(
        term.status().contains("fetching threads"),
        "the prepare goes on: {}",
        term.status()
    );
    term.send("a");
    term.send("\r");
    term.wait_for("the prompt", |t| t.contains("2 comments weren't posted"));
    assert!(
        term.contains("Line could not be resolved"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("s");
    term.wait_for("the submit", |t| {
        t.status().contains("approved · 2 comments not posted")
    });
    let sent = std::fs::read_to_string(&args).expect("args");
    assert_eq!(sent.trim(), "post 7 --apply-event --event approve");
    term.send("q");
    assert!(term.wait_exit().success());
}

/// What a watching viewer needs: `watch = true` in its config, and a
/// private runtime directory so it gets a broker of its own, stopped on
/// drop.
struct Watching {
    config: tempfile::TempDir,
    runtime: tempfile::TempDir,
    /// Outside the watched repository, so cache writes aren't changes to it.
    cache: tempfile::TempDir,
}

impl Watching {
    fn new() -> Self {
        Self::with_config("watch = true\n")
    }

    /// A private broker for a viewer with `config` as its config file.
    fn with_config(text: &str) -> Self {
        Self {
            config: config(text),
            runtime: tempfile::tempdir().expect("tempdir"),
            cache: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn env(&self) -> [(&str, &str); 4] {
        [
            (
                "XDG_CONFIG_HOME",
                self.config.path().to_str().expect("utf-8"),
            ),
            ("XDG_CACHE_HOME", self.cache.path().to_str().expect("utf-8")),
            (
                "XDG_RUNTIME_DIR",
                self.runtime.path().to_str().expect("utf-8"),
            ),
            ("DFFY_DAEMON_IDLE_SECS", "1"),
        ]
    }

    fn dffy(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dffy"));
        cmd.args(args)
            .envs(self.env())
            .env("XDG_STATE_HOME", self.runtime.path().join("state"));
        cmd
    }
}

impl Drop for Watching {
    fn drop(&mut self) {
        self.dffy(&["daemon", "stop"]).output().ok();
    }
}

fn percentile(sorted: &[Duration], p: usize) -> Duration {
    sorted[(sorted.len() * p / 100).min(sorted.len() - 1)]
}

/// Edits a file the viewer shows, over and over, timing each edit from the
/// write to the frame showing it. A second subscriber timestamps when the
/// broker delivered the debounced change, which splits the time into the
/// debounce and dffy's own refresh.
#[test]
fn watch_mode_refreshes_an_edited_file_in_place() {
    const EDITS: usize = 25;
    let repo = repo();
    let watching = Watching::new();
    let mut term = Term::spawn(repo.path(), 120, 30, &watching.env());
    term.wait_for("the diffs and the watch", |t| {
        t.contains("More.") && t.status().contains(" watch ")
    });
    let mut subscriber = watching
        .dffy(&["daemon", "watch"])
        .arg(repo.path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("subscriber");
    let (tx, notices) = std::sync::mpsc::channel();
    let stdout = subscriber.stdout.take().expect("stdout");
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send((Instant::now(), line)).is_err() {
                break;
            }
        }
    });
    let (_, ready) = notices.recv_timeout(TIMEOUT).expect("subscriber ready");
    assert!(ready.starts_with("ready"), "{ready}");
    // Let the rescan that follows the watch starting finish.
    term.settle();

    let readme = repo.path().join("README.md");
    let (mut total, mut refresh) = (Vec::new(), Vec::new());
    for i in 0..EDITS {
        let token = format!("edit-{i:03}");
        std::fs::write(&readme, format!("# demo\n\n{token}\n")).expect("write");
        let written = Instant::now();
        let shown = loop {
            let at = Instant::now();
            term.update();
            if term.contains(&token) {
                break at;
            }
            assert!(written.elapsed() < TIMEOUT, "edit {i} never showed");
            std::thread::sleep(Duration::from_micros(250));
        };
        let delivered = loop {
            let (at, line) = notices.recv_timeout(TIMEOUT).expect("the notice");
            if line.ends_with("README.md") {
                break at;
            }
        };
        total.push(shown - written);
        refresh.push(shown.saturating_duration_since(delivered));
        std::thread::sleep(Duration::from_millis(30));
    }
    total.sort();
    refresh.sort();
    eprintln!(
        "{EDITS} edits, write to repaint: median {:?}, p95 {:?}, max {:?}; \
         delivered notice to repaint: median {:?}, p95 {:?}, max {:?}",
        percentile(&total, 50),
        percentile(&total, 95),
        total[EDITS - 1],
        percentile(&refresh, 50),
        percentile(&refresh, 95),
        refresh[EDITS - 1],
    );
    subscriber.kill().ok();
    subscriber.wait().ok();
    term.send("q");
    assert!(term.wait_exit().success());
}

#[test]
fn watch_mode_keeps_the_position_and_waits_through_an_empty_diff() {
    let repo = repo();
    let watching = Watching::new();
    let mut term = Term::spawn(repo.path(), 100, 12, &watching.env());
    term.wait_for("the diffs and the watch", |t| {
        t.contains(" +3 -3") && t.status().contains(" watch ")
    });
    term.settle();
    term.send("nnn");
    assert!(term.row(1).contains("▌ line 100"), "{}", term.row(1));

    // An edit further down the file shown: the counts change, the top row
    // doesn't move.
    let lib = repo.path().join("src/lib.rs");
    let text = std::fs::read_to_string(&lib).expect("read");
    std::fs::write(&lib, text.replace("line 191\n", "line 191\nline 191½\n")).expect("write");
    term.wait_for("the refreshed counts", |t| t.row(0).contains(" +4 -3"));
    assert!(term.row(1).contains("▌ line 100"), "{}", term.row(1));

    // A commit empties the diff; the viewer stays and says so.
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "everything"]);
    term.wait_for("the empty diff", |t| t.contains("no changes — watching"));
    status_shows(&mut term, " watch ");

    std::fs::write(repo.path().join("fresh.txt"), "fresh\n").expect("write");
    term.wait_for("the new file", |t| {
        t.contains("fresh.txt") && t.contains("fresh")
    });

    term.send("w");
    assert!(!term.status().contains(" watch "), "{}", term.status());
    term.send("q");
    assert!(term.wait_exit().success());
}

#[test]
fn an_empty_diff_prints_and_exits() {
    let repo = repo();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "everything"]);
    let mut term = Term::spawn(repo.path(), 80, 24, &[]);
    let status = term.wait_exit();
    assert!(status.success(), "{status}");
    let out = String::from_utf8_lossy(&term.output()).into_owned();
    assert!(out.contains("no changes"), "{out:?}");
    assert!(!out.contains("\x1b[?1049h"), "took the screen: {out:?}");

    // Staged work isn't "no changes": `--unstaged` and `--staged` say what
    // the other side holds.
    let d = repo.path();
    std::fs::write(d.join("staged.txt"), "s\n").expect("write");
    git(d, &["add", "staged.txt"]);
    std::fs::write(d.join("untracked.txt"), "u\n").expect("write");
    let printed = |flag: &str| {
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_dffy"));
        cmd.arg(flag);
        let mut term = Term::spawn_command(cmd, d, 80, 24, &[]);
        assert!(term.wait_exit().success());
        String::from_utf8_lossy(&term.output()).into_owned()
    };
    let out = printed("--unstaged");
    assert!(
        out.contains("no unstaged changes (1 staged, 1 untracked; run dffy to see them)"),
        "{out:?}"
    );
    git(d, &["commit", "-q", "-m", "staged"]);
    let out = printed("--staged");
    assert!(
        out.contains("no staged changes (1 untracked; run dffy to see them)"),
        "{out:?}"
    );
}

/// Staging and unstaging in watch mode reword the title as the index
/// changes; the tree keeps the bare letter.
#[test]
fn watch_mode_follows_staging() {
    let repo = repo();
    let d = repo.path();
    let watching = Watching::new();
    let mut term = Term::spawn(d, 120, 20, &watching.env());
    let shows = |t: &Term, word: &str| {
        t.row(0)
            .contains(&format!("modified · {word} · README.md · +2 -0"))
    };
    term.wait_for("unstaged", |t| {
        shows(t, "unstaged") && t.status().contains(" watch ")
    });
    assert!(
        term.contains("│ M README.md"),
        "{}",
        term.vt.screen().contents()
    );
    term.settle();
    git(d, &["add", "README.md"]);
    term.wait_for("staged", |t| shows(t, "staged"));
    std::fs::write(d.join("README.md"), "# demo\n\nEdited after staging.\n").expect("write");
    term.wait_for("partly staged", |t| shows(t, "partly staged"));
    git(d, &["reset", "-q", "README.md"]);
    term.wait_for("unstaged again", |t| shows(t, "unstaged"));
    term.send("q");
    assert!(term.wait_exit().success());
}

/// `d` switches to the syntax diff, which arrives after the line diff has
/// painted: the moved function then shows as moved (`»`). Keys keep
/// working while it's computed, and quitting stays prompt.
#[test]
fn d_toggles_the_syntax_diff_without_blocking_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    let add = "static int add(int a, int b)\n{\n\treturn a + b;\n}\n";
    let sub = "static int sub(int a, int b)\n{\n\treturn a - b;\n}\n";
    std::fs::write(d.join("math.c"), format!("{add}\n{sub}")).expect("write");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    std::fs::write(d.join("math.c"), format!("{sub}\n{add}")).expect("write");
    // Signs rather than bars, to read the markers back as text.
    let config = config("indicators = classic\n");
    let mut term = Term::spawn(d, 100, 20, &[("XDG_CONFIG_HOME", &path(&config))]);
    term.wait_for("the line diff", |t| t.contains("-│ static int"));
    assert!(!term.status().contains("syntax"), "{}", term.status());
    assert!(!term.contains("»│ static int"));

    term.send("d");
    assert!(term.status().contains("syntax"), "{}", term.status());
    term.wait_for("the syntax diff", |t| t.contains("»│ static int"));
    assert!(!term.contains("-│ static int"));

    term.send("djk");
    assert!(!term.status().contains("syntax"), "{}", term.status());
    assert!(term.contains("-│ static int"));
    // Back on, the result is kept: no recomputing.
    term.writer.write_all(b"dq").expect("keys");
    term.writer.flush().expect("flush");
    let asked = Instant::now();
    assert!(term.wait_exit().success());
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "{:?}",
        asked.elapsed()
    );
}

/// Files arrive as an addition and a deletion, then the rename pass merges
/// them into one renamed entry; its code is syntax-colored.
#[test]
fn renames_merge_and_code_is_highlighted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    let body = (0..30)
        .map(|i| format!("fn f{i}() {{}}\n"))
        .collect::<Vec<_>>()
        .concat();
    std::fs::write(d.join("old.rs"), &body).expect("write");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    std::fs::remove_file(d.join("old.rs")).expect("rm");
    std::fs::write(d.join("new.rs"), body.replace("fn f3()", "fn three()")).expect("write");
    // Intent to add, so the new file is an addition in the worktree diff.
    git(d, &["add", "-N", "new.rs"]);
    let mut term = Term::spawn(d, 120, 20, &[]);
    term.wait_for("the rename", |t| {
        t.row(0).contains("renamed · unstaged · old.rs → new.rs")
    });
    assert!(term.status().contains(" 1/1"), "{}", term.status());
    term.wait_for("a keyword in its syntax color", |t| {
        let screen = t.vt.screen();
        (0..20).any(|y| {
            (0..120).any(|x| {
                screen
                    .cell(y, x)
                    .is_some_and(|c| c.contents() == "f" && c.fgcolor() == vt100::Color::Idx(5))
            })
        })
    });
    term.send("q");
    assert!(term.wait_exit().success());
}

/// Files in nested directories, each with a different count, plus an
/// untracked one: the tree shows
///
/// ```text
/// M README.md
/// ▼ docs/guide/
///   M intro.md
/// ▼ src/
///   ▼ app/
///     M main.rs
///     ? new.rs
///     M view.rs
///   M lib.rs
/// ```
fn tree_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    let lines = |n: usize, word: &str| {
        (1..=n)
            .map(|i| format!("{word} {i}\n"))
            .collect::<Vec<_>>()
            .concat()
    };
    let files = [
        ("README.md", lines(3, "readme")),
        ("docs/guide/intro.md", lines(5, "intro")),
        ("src/app/main.rs", lines(8, "main")),
        ("src/app/view.rs", lines(4, "view")),
        ("src/lib.rs", lines(2, "lib")),
    ];
    for (path, text) in &files {
        let full = d.join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        std::fs::write(full, text).expect("write");
    }
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    for (path, text) in files {
        let changed = match path {
            "README.md" => format!("{text}more\n"),
            "docs/guide/intro.md" => text.replace("intro 2\n", "two\nthree\n"),
            "src/app/main.rs" => text.replace("main 3\nmain 4\n", "three\nfour\nfive\n"),
            "src/app/view.rs" => text.replace("view 1\n", "one\n"),
            _ => format!("{text}{}", lines(10, "more")),
        };
        std::fs::write(d.join(path), changed).expect("write");
    }
    std::fs::write(d.join("src/app/new.rs"), lines(5, "new")).expect("write");
    dir
}

/// The tree panel's rows below its top border, the frame's, as text.
fn tree_rows(term: &Term, width: u16) -> Vec<String> {
    let (rows, _) = term.vt.screen().size();
    (1..rows.get())
        .map(|y| {
            term.vt
                .screen()
                .contents_between(y, 0, y, width)
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// The tree row that holds `name`, if shown.
fn tree_row(term: &Term, name: &str) -> Option<u16> {
    (1..20).find(|&y| {
        term.vt
            .screen()
            .contents_between(y, 0, y, 30)
            .contains(name)
    })
}

#[test]
fn tree_keys_move_open_collapse_mark_filter_and_hide() {
    let repo = tree_repo();
    let mut term = Term::spawn(repo.path(), 120, 20, &[]);
    term.wait_for("the tree with counts", |t| {
        t.row(0).contains("Files · 6") && t.contains("+10")
    });
    let border = |t: &Term| t.vt.screen().cell(2, 0).map(vt100::Cell::fgcolor);
    assert_ne!(border(&term), Some(vt100::Color::Idx(6)));

    // tab: the tree takes the keys; its border says so.
    term.send("\t");
    assert_eq!(border(&term), Some(vt100::Color::Idx(6)));
    // A folder row doesn't move the diff; a file row does.
    term.send("j");
    assert!(term.status().contains(" 1/6 "), "{}", term.status());
    term.send("j");
    assert!(term.status().contains(" 2/6 "), "{}", term.status());
    assert!(
        term.row(0).contains("docs/guide/intro.md"),
        "{}",
        term.row(0)
    );

    // h: to the folder, then closed; l opens it again.
    term.send("h");
    term.send("h");
    assert!(
        tree_row(&term, "intro.md").is_none(),
        "{:#?}",
        tree_rows(&term, 30)
    );
    assert!(tree_row(&term, "▶ docs/guide/").is_some());
    term.send("l");
    assert!(
        tree_row(&term, "intro.md").is_some(),
        "{:#?}",
        tree_rows(&term, 30)
    );

    // v: viewed, dimmed.
    term.send("jv");
    let y = tree_row(&term, "intro.md").expect("intro.md");
    let row = term.vt.screen().contents_between(y, 0, y, 30);
    let name = row.find("intro").expect("name");
    let x = u16::try_from(row[..name].chars().count()).expect("column");
    assert!(term.vt.screen().cell(y, x).is_some_and(vt100::Cell::dim));

    // /: only matching paths, and the diff goes to the first.
    term.send("/view\r");
    assert!(tree_row(&term, "view.rs").is_some());
    assert!(
        tree_row(&term, "main.rs").is_none(),
        "{:#?}",
        tree_rows(&term, 30)
    );
    assert!(term.row(0).contains("src/app/view.rs"), "{}", term.row(0));
    term.send("\x1b");
    assert!(
        tree_row(&term, "main.rs").is_some(),
        "{:#?}",
        tree_rows(&term, 30)
    );

    // enter on a file opens it and hands the keys to the diff.
    term.send("k\r");
    assert!(term.row(0).contains("src/app/new.rs"), "{}", term.row(0));
    assert_ne!(border(&term), Some(vt100::Color::Idx(6)));

    // A click on a row opens that file.
    let y = tree_row(&term, "lib.rs").expect("lib.rs");
    term.send(&format!("\x1b[<0;5;{}M", y + 1));
    assert!(term.row(0).contains("src/lib.rs"), "{}", term.row(0));

    term.send("T");
    assert!(!term.row(0).starts_with("┌ Files"), "{}", term.row(0));
    term.send("q");
    assert!(term.wait_exit().success());
}

/// Moving between files changes nothing in the tree but the highlight,
/// once every count is in.
#[test]
fn the_tree_doesnt_reflow_between_file_moves() {
    let repo = tree_repo();
    let mut term = Term::spawn(repo.path(), 120, 20, &[]);
    term.wait_for("every count", |t| {
        t.row(0).contains("Files · 6 +22 -4") && t.contains("+10")
    });
    term.settle();
    // The default theme's selection is a bright black background.
    let highlighted = |t: &Term| {
        (2..20).find(|&y| {
            t.vt.screen()
                .cell(y, 3)
                .is_some_and(|c| c.bgcolor() == vt100::Color::Idx(8))
        })
    };
    let before = tree_rows(&term, 30);
    let start = highlighted(&term);
    term.send("f");
    let after_one = tree_rows(&term, 30);
    let moved = highlighted(&term);
    term.send("f");
    let after_two = tree_rows(&term, 30);
    assert_eq!(before, after_one);
    assert_eq!(after_one, after_two);
    assert_ne!(start, moved);
    assert_ne!(moved, highlighted(&term));
    term.send("q");
    assert!(term.wait_exit().success());
}

/// `x` and a click open a gap 20 lines at a time; `X` shows the whole file
/// and back, keeping what was opened.
#[test]
fn gaps_open_with_x_and_a_click_and_x_shows_the_whole_file() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 100, 45, &[]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    term.send("f");
    term.wait_for("the gap", |t| t.contains("⋯ 90 unchanged lines"));
    term.send("x");
    assert!(
        term.contains("⋯ 70 unchanged lines"),
        "{}",
        term.vt.screen().contents()
    );
    let y = (0..45)
        .find(|&y| term.row(y).contains("⋯ 70"))
        .expect("the separator");
    term.send(&format!("\x1b[<0;40;{}M", y + 1));
    assert!(
        term.contains("⋯ 50 unchanged lines"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("X");
    assert!(
        !term.contains("unchanged lines"),
        "{}",
        term.vt.screen().contents()
    );
    assert!(term.status().contains(" full"), "{}", term.status());
    term.send("gX");
    assert!(
        term.contains("⋯ 50 unchanged lines"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("q");
    assert!(term.wait_exit().success());
}

/// A phone: 50 columns, 15 rows. Letters alone get everywhere; the tree
/// opens over the diff and closes once a file is picked.
#[test]
fn a_phone_sized_terminal_works_with_letters_alone() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 50, 15, &[]);
    term.wait_for("the first file", |t| t.contains("More."));
    assert!(!term.row(1).starts_with('┌'), "{}", term.row(1));
    term.send("n");
    assert!(
        term.contains("3 ▌ More."),
        "{}",
        term.vt.screen().contents()
    );
    term.send("n");
    assert!(
        term.status().contains("next file: src/lib.rs"),
        "{}",
        term.status()
    );
    assert!(term.contains("▌ line 3"), "{}", term.vt.screen().contents());
    term.send("F");
    assert!(term.contains("More."), "{}", term.vt.screen().contents());
    term.send("f");
    assert!(term.contains("▌ line 3"), "{}", term.vt.screen().contents());
    assert!(
        term.contains("⋯ 90 unchanged lines"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("x");
    assert!(!term.contains("⋯ 90"), "{}", term.vt.screen().contents());
    assert!(term.contains("line 11"), "{}", term.vt.screen().contents());
    term.send("X");
    assert!(
        !term.contains("unchanged lines"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("\t");
    assert!(term.row(1).starts_with("┌ Files · 2"), "{}", term.row(1));
    // Up past `src/` to README.md, and open it.
    term.send("kk\r");
    assert!(!term.row(1).starts_with('┌'), "{}", term.row(1));
    assert!(term.row(0).contains("1/2"), "{}", term.row(0));
    for y in 0..15 {
        assert!(term.row(y).chars().count() <= 50, "{}", term.row(y));
    }
    term.send("q");
    assert!(term.wait_exit().success());
}

/// `v` selects from the first line of code on screen, `j` extends it, and
/// `y` sends exactly that code to the clipboard as one OSC 52 sequence:
/// no line numbers, no signs, no trailing line break.
#[test]
fn v_j_y_copies_the_selected_code_through_osc_52() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 100, 20, &[]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    term.send("f");
    term.wait_for("src/lib.rs", |t| t.row(0).contains("src/lib.rs"));
    term.send("vjj");
    assert!(term.status().contains("3 new lines"), "{}", term.status());
    let before = term.output().len();
    term.send("y");
    let out = term.output();
    let sequence = b"\x1b]52;c;bGluZSAxCmxpbmUgMgpsaW5lIHRocmVl\x07";
    assert!(
        out[before..].windows(sequence.len()).any(|w| w == sequence),
        "{:?}",
        String::from_utf8_lossy(&out[before..])
    );
    assert!(
        term.status().contains("copied 3 lines"),
        "{}",
        term.status()
    );
    // A change `n` lands on is the first row under the frame's top, which
    // names the file and the hunk.
    term.send("n");
    assert!(
        term.row(0).contains("src/lib.rs · +3 -3"),
        "{}",
        term.row(0)
    );
    assert!(term.row(0).contains("@@ -1,6 +1,6 @@"), "{}", term.row(0));
    assert!(term.row(1).contains("▌ line 3"), "{}", term.row(1));
    term.send("q");
    assert!(term.wait_exit().success());
}

/// The screen cell (0-based column, row) where `text` starts on a row
/// that has it.
fn cell_of(term: &Term, text: &str) -> (usize, u16) {
    let (_, rows) = term.vt.screen().size();
    (0..rows.get())
        .find_map(|y| {
            let row = term.row(y);
            row.find(text).map(|i| (row[..i].chars().count(), y))
        })
        .unwrap_or_else(|| panic!("{text} on screen:\n{}", term.vt.screen().contents()))
}

/// Whether the output since `before` holds `sequence`.
fn sent(term: &Term, before: usize, sequence: &[u8]) -> bool {
    term.output()[before..]
        .windows(sequence.len())
        .any(|w| w == sequence)
}

/// A drag over code selects by characters and copies on release, only
/// the code; a double click copies the word under it.
#[test]
fn dragging_over_code_copies_exactly_the_text_under_it() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 100, 20, &[]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    term.send("f");
    term.wait_for("src/lib.rs", |t| t.contains("line three"));
    let (x1, y1) = cell_of(&term, "line 1");
    let (x2, y2) = cell_of(&term, "line 2");
    let before = term.output().len();
    let (x2, x1) = (x2 + 6, x1 + 1);
    term.send(&format!(
        "\x1b[<0;{x1};{}M\x1b[<32;{x2};{}M\x1b[<0;{x2};{}m",
        y1 + 1,
        y2 + 1,
        y2 + 1
    ));
    // `line 1\nline 2`, numbers and markers left out.
    assert!(
        sent(&term, before, b"\x1b]52;c;bGluZSAxCmxpbmUgMg==\x07"),
        "{:?}",
        String::from_utf8_lossy(&term.output()[before..])
    );
    assert!(
        term.status().contains("copied 2 lines"),
        "{}",
        term.status()
    );
    let (x, y) = cell_of(&term, "three");
    let before = term.output().len();
    let click = format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", x + 3, y + 1, x + 3, y + 1);
    term.send(&format!("{click}{click}"));
    assert!(
        sent(&term, before, b"\x1b]52;c;dGhyZWU=\x07"),
        "{:?}",
        String::from_utf8_lossy(&term.output()[before..])
    );
    assert!(
        term.status().contains("copied 5 chars"),
        "{}",
        term.status()
    );
    term.send("q");
    assert!(term.wait_exit().success());
}

/// `M` in the command panel turns the terminal's mouse reports off, for
/// its own selection, and back on.
#[test]
fn m_hands_the_mouse_to_the_terminal_and_back() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 100, 20, &[]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    let before = term.output().len();
    term.send(" M");
    assert!(sent(&term, before, b"\x1b[?1006l\x1b[?1002l\x1b[?1000l"));
    term.send("\x1b");
    term.wait_for("the note", |t| t.status().contains("mouse off (space M)"));
    let before = term.output().len();
    term.send(" M");
    assert!(sent(&term, before, b"\x1b[?1000h\x1b[?1002h\x1b[?1006h"));
    assert!(!term.status().contains("mouse off"), "{}", term.status());
    term.send("\x1b");
    term.send("q");
    assert!(term.wait_exit().success());
}

/// `>` and `<` scroll long lines sideways, `…` marking the cut; `0` goes
/// back to the first column.
#[test]
fn angle_brackets_scroll_long_lines_sideways() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    let long = "let value = compute(first_argument, second_argument, third_argument, fourth_argument, fifth);\n";
    std::fs::write(d.join("wide.rs"), long).expect("write");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    std::fs::write(d.join("wide.rs"), long.replace("fifth", "sixth")).expect("write");
    let mut term = Term::spawn(d, 60, 15, &[]);
    term.wait_for("the diff", |t| t.contains("let value = comp"));
    term.send(">");
    assert!(
        term.contains("…e = compute(first_argument"),
        "{}",
        term.vt.screen().contents()
    );
    term.send(">>");
    assert!(
        term.contains("…t_argument, second"),
        "{}",
        term.vt.screen().contents()
    );
    assert!(
        !term.contains("compute("),
        "{}",
        term.vt.screen().contents()
    );
    term.send("<");
    assert!(
        term.contains("…ute(first_argument"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("0");
    assert!(
        term.contains("let value = comp"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("q");
    assert!(term.wait_exit().success());
}

#[test]
fn a_remapped_key_runs_its_action() {
    let repo = repo();
    let config = config("keybind = J=next_file\nkeybind = q=unbind\nkeybind = Q=quit\n");
    let mut term = Term::spawn(repo.path(), 100, 12, &[("XDG_CONFIG_HOME", &path(&config))]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    term.send("J");
    assert!(term.status().contains(" 2/2"), "{}", term.status());
    term.send("q");
    assert!(
        term.child.try_wait().expect("wait").is_none(),
        "q no longer quits"
    );
    term.send("Q");
    assert!(term.wait_exit().success());
}

/// `copy_absolute_path`, bound to a key, copies where the file shown is
/// on disk, from the root, though dffy runs in the repository.
#[test]
fn the_absolute_path_is_copied_from_the_root() {
    let repo = repo();
    let config = config("keybind = K=copy_absolute_path\n");
    let mut term = Term::spawn(repo.path(), 200, 12, &[("XDG_CONFIG_HOME", &path(&config))]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    term.send("f");
    term.wait_for("src/lib.rs", |t| t.row(0).contains("src/lib.rs"));
    let before = term.output().len();
    term.send("K");
    let status = term.status();
    let copied = status
        .split("copied ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or_else(|| panic!("{status}"));
    assert!(
        copied.starts_with('/') && copied.ends_with("/src/lib.rs"),
        "{status}"
    );
    assert!(
        sent(&term, before, b"\x1b]52;c;"),
        "{:?}",
        String::from_utf8_lossy(&term.output()[before..])
    );
    term.send("q");
    assert!(term.wait_exit().success());
}

/// The cell background of the first added line's text.
fn added_tint(term: &Term) -> Option<vt100::Color> {
    let screen = term.vt.screen();
    let y = (0..20).find(|&y| term.row(y).contains("More."))?;
    let x = u16::try_from(term.row(y).find("More.")?).ok()?;
    screen.cell(y, x).map(vt100::Cell::bgcolor)
}

/// On a truecolor terminal changed lines are tinted, dark until the
/// terminal answers the background query with a light color.
#[test]
fn a_light_background_reply_repaints_with_light_tints() {
    let repo = repo();
    // Green mixed into the assumed dark background, then into white.
    let dark = vt100::Color::Rgb(0x25, 0x32, 0x35);
    let light = vt100::Color::Rgb(0xe4, 0xf3, 0xe4);

    // No reply: dark, and drawn without waiting for one.
    let mut term = Term::spawn(repo.path(), 100, 20, &[("COLORTERM", "truecolor")]);
    term.wait_for("the dark tints", |t| added_tint(t) == Some(dark));
    assert!(term.output().windows(7).any(|w| w == b"\x1b]11;?\x1b"));
    term.send("q");
    assert!(term.wait_exit().success());

    let mut term = Term::spawn(repo.path(), 100, 20, &[("COLORTERM", "truecolor")]);
    term.wait_for("the query", |t| {
        t.output().windows(7).any(|w| w == b"\x1b]11;?\x1b")
    });
    term.writer
        .write_all(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\")
        .expect("reply");
    term.writer.flush().expect("flush");
    term.wait_for("the light tints", |t| added_tint(t) == Some(light));
    term.send("q");
    assert!(term.wait_exit().success());
}

/// A diff of `files` one-hunk files, `dir<n % 3>/f<n>.txt`, as git prints
/// it.
fn many_files_diff(files: usize) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for i in 0..files {
        let path = format!("dir{}/f{i}.txt", i % 3);
        let _ = write!(
            out,
            "diff --git a/{path} b/{path}\nindex 1111111..2222222 100644\n--- a/{path}\n\
             +++ b/{path}\n@@ -10,3 +10,3 @@ fn f{i}()\n a\n-old {i}\n+new {i}\n c\n"
        );
    }
    out
}

/// A short diff piped to dffy on a terminal is printed, as `less -F`
/// would, without taking the screen.
#[test]
fn a_short_piped_diff_prints_without_the_viewer() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("short.diff"), many_files_diff(1)).expect("write");
    let mut term = Term::spawn_sh(dir.path(), 100, 24, "cat short.diff | \"$DFFY\"");
    assert!(term.wait_exit().success());
    let out = String::from_utf8_lossy(&term.output()).into_owned();
    assert!(!out.contains("\x1b[?1049h"), "took the screen: {out:?}");
    assert!(term.contains("modified: dir0/f0.txt"), "{out:?}");
    assert!(term.contains("new 0"), "{out:?}");
}

/// With `paging = always`, even a diff that fits one screen opens the
/// viewer.
#[test]
fn paging_always_opens_a_short_piped_diff_in_the_viewer() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("short.diff"), many_files_diff(1)).expect("write");
    let mut term = Term::spawn_sh(
        dir.path(),
        100,
        24,
        "cat short.diff | \"$DFFY\" --paging=always",
    );
    term.wait_for("the viewer", |t| {
        t.vt.screen().alternate_screen() && t.contains("new 0")
    });
    term.send("q");
    assert!(term.wait_exit().success());
    assert!(!term.vt.screen().alternate_screen());
}

/// A diff longer than the screen opens the viewer, on the keys from the
/// terminal while the diff comes from the pipe; quitting restores the
/// terminal. Piped files have only their hunks: `x` says so.
#[test]
fn a_long_piped_diff_opens_the_viewer() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("long.diff"), many_files_diff(40)).expect("write");
    let mut term = Term::spawn_sh(dir.path(), 100, 20, "cat long.diff | \"$DFFY\"");
    term.wait_for("the viewer", |t| t.row(0).starts_with("┌ Files · 40"));
    assert!(term.vt.screen().alternate_screen());
    term.wait_for("the first diff", |t| t.contains("old 0"));
    assert!(term.status().contains(" 1/40 "), "{}", term.status());
    assert!(
        term.contains("⋯ 9 unchanged lines  @@ -10,3 +10,3 @@ fn f0()"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("n");
    term.send("n");
    assert!(term.status().contains(" 2/40 "), "{}", term.status());
    assert!(
        term.status().contains("next file: dir1/f1.txt"),
        "{}",
        term.status()
    );
    term.wait_for("the second diff", |t| t.contains("new 1"));
    term.send("f");
    assert!(term.status().contains(" 3/40 "), "{}", term.status());
    term.send("x");
    assert!(
        term.status().contains("only its hunks"),
        "{}",
        term.status()
    );
    term.send("q");
    assert!(term.wait_exit().success());
    assert!(!term.vt.screen().alternate_screen());
    assert!(!term.vt.screen().hide_cursor());
}

/// Input that isn't a diff goes through untouched, byte for byte: with
/// paging off, and through a pager (`cat -` here; plain `cat` means no
/// pager) with paging auto or always.
#[test]
fn piped_input_that_isnt_a_diff_passes_through_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut input =
        b"* main\n  feature\n\x1b[31mred\x1b[m and \xc3\xa9t\xc3\xa9\n\xff\xfe raw bytes\n"
            .to_vec();
    input.extend_from_slice("    --- not a diff\n".repeat(300).as_bytes());
    std::fs::write(dir.path().join("in.txt"), &input).expect("write");
    for script in [
        "cat in.txt | \"$DFFY\" --paging=never",
        "cat in.txt | DFFY_PAGER='cat -' \"$DFFY\"",
        "cat in.txt | DFFY_PAGER='cat -' \"$DFFY\" --paging=always",
    ] {
        let mut term = Term::spawn_sh(dir.path(), 80, 24, script);
        assert!(term.wait_exit().success(), "{script}");
        // The terminal turns each newline into a carriage return and one.
        let out = term.output();
        let mut lf = Vec::with_capacity(out.len());
        let mut i = 0;
        while i < out.len() {
            if out[i..].starts_with(b"\r\n") {
                i += 1;
            }
            lf.push(out[i]);
            i += 1;
        }
        assert_eq!(
            String::from_utf8_lossy(&lf),
            String::from_utf8_lossy(&input),
            "{script}"
        );
        assert_eq!(lf, input, "{script}");
    }
}

fn git_out(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?} failed");
    out.stdout
}

/// A repository with three commits, each changing a file, and their
/// `git log -p` in `log.diff`, outside the working tree.
fn three_commits() -> (tempfile::TempDir, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    let lines: Vec<String> = (1..=40).map(|i| format!("line {i}\n")).collect();
    std::fs::create_dir(d.join("src")).expect("mkdir");
    std::fs::write(d.join("src/a.rs"), lines.concat()).expect("write");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "First commit\n\nWith a body."]);
    for (n, (file, subject)) in [("src/a.rs", "Second commit"), ("b.txt", "Third commit")]
        .into_iter()
        .enumerate()
    {
        let mut changed = lines.clone();
        changed[20] = format!("changed {n}\n");
        std::fs::write(d.join(file), changed.concat()).expect("write");
        git(d, &["add", "-A"]);
        git(d, &["commit", "-q", "-m", subject]);
    }
    let out = tempfile::tempdir().expect("tempdir");
    let log = git_out(d, &["-c", "color.ui=always", "log", "-p"]);
    std::fs::write(out.path().join("log.diff"), log).expect("write");
    (dir, out)
}

/// `git log -p` in the viewer: each commit a node in the tree with its
/// files under it, its header shown when it's picked, and the status bar
/// counting commits. `dffy show` shows the same header.
#[test]
fn a_piped_log_shows_commits_and_their_headers() {
    let (repo, out) = three_commits();
    let mut term = Term::spawn_sh(out.path(), 120, 30, "cat log.diff | \"$DFFY\"");
    term.wait_for("the commits", |t| {
        t.contains("First commit") && t.contains("Third commit")
    });
    assert!(term.vt.screen().alternate_screen());
    let tree = tree_rows(&term, 36).join("\n");
    for row in [
        "Third commit",
        "  A b.txt",
        "Second commit",
        "  ▼ src/",
        "First commit",
    ] {
        assert!(tree.contains(row), "{row}: {tree}");
    }
    assert!(
        term.status().contains(" commit 1/3  1 file "),
        "{}",
        term.status()
    );
    assert!(term.contains("Author: Ada Lovelace <ada@example.com>"));
    assert!(term.contains("    Third commit"));
    assert!(
        term.contains("  A b.txt +40"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("f");
    assert!(
        term.status().contains(" commit 1/3 ") && term.status().contains(" 1/1 · "),
        "{}",
        term.status()
    );
    term.wait_for("the file", |t| t.row(0).contains("new file · b.txt"));
    // b.txt is new, its one change at the top already: `n` goes on into
    // the next commit's.
    term.send("n");
    assert!(term.status().contains("commit 2/3"), "{}", term.status());
    assert!(term.status().contains("Second commit"), "{}", term.status());
    term.wait_for("the change", |t| t.contains("changed 0"));
    term.send("q");
    assert!(term.wait_exit().success());

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_dffy"));
    cmd.args(["show", "HEAD~1"]);
    let mut term = Term::spawn_command(cmd, repo.path(), 120, 30, &[]);
    term.wait_for("the header", |t| t.contains("    Second commit"));
    assert!(term.vt.screen().alternate_screen());
    // The file is counted after the first frame; until then the header
    // holds a placeholder, never a total.
    term.wait_for("the counts", |t| {
        assert!(!t.contains("1 file +0 -0"), "{}", t.vt.screen().contents());
        t.contains("1 file +1 -1")
    });
    assert!(term.status().contains("show HEAD~1"), "{}", term.status());
    term.send("f");
    term.wait_for("the file", |t| t.contains("changed 0"));
    term.send("q");
    assert!(term.wait_exit().success());
}

/// An endless writer: dffy reads only a window ahead of the view, so the
/// writer blocks on the full pipe, and quitting ends both promptly with
/// no broken-pipe message.
#[test]
fn an_endless_piped_diff_is_read_only_ahead_of_the_view() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = "i=0; while :; do \
        printf 'diff --git a/f%d b/f%d\\n--- a/f%d\\n+++ b/f%d\\n@@ -1 +1 @@\\n-a\\n+b\\n' $i $i $i $i; \
        i=$((i+1)); done | \"$DFFY\"; echo \"exit=$?\"";
    let mut term = Term::spawn_sh(dir.path(), 100, 20, script);
    term.wait_for("the viewer", |t| t.row(0).starts_with("┌ Files"));
    std::thread::sleep(Duration::from_millis(500));
    term.settle();
    assert!(term.status().contains(" 1/128+ "), "{}", term.status());
    // Moving on reads on.
    term.send("ff");
    term.wait_for("more", |t| t.status().contains(" 3/130+ "));
    let asked = Instant::now();
    term.send("q");
    assert!(term.wait_exit().success());
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "{:?}",
        asked.elapsed()
    );
    assert!(term.contains("exit=0"), "{}", term.vt.screen().contents());
    let out = String::from_utf8_lossy(&term.output()).to_lowercase();
    assert!(!out.contains("broken pipe"), "{out}");
}

/// Writes `keys` without waiting for output to settle: a header counting
/// up never settles.
fn type_raw(term: &mut Term, keys: &str) {
    term.writer.write_all(keys.as_bytes()).expect("send keys");
    term.writer.flush().expect("flush keys");
}

/// The picker reads a piped diff to its end while it's open: its header
/// counts the files as they come, the last file can be found and opened,
/// and the count ends whole once the input does.
#[test]
fn the_picker_reads_a_piped_diff_to_its_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let diff = many_files_diff(3000);
    // Split at a file boundary, the second half held back a while.
    let half = diff.find("diff --git a/dir0/f1500.txt").expect("file 1500");
    std::fs::write(dir.path().join("a.diff"), &diff[..half]).expect("write");
    std::fs::write(dir.path().join("b.diff"), &diff[half..]).expect("write");
    let mut term = Term::spawn_sh(
        dir.path(),
        100,
        20,
        "{ cat a.diff; while [ ! -e go ]; do sleep 0.05; done; cat b.diff; } | \"$DFFY\"",
    );
    term.wait_for("the window read ahead", |t| t.status().contains(" 1/128+ "));
    type_raw(&mut term, "o");
    // The first half's last file isn't known to be whole until the next
    // one starts.
    term.wait_for("reading progress", |t| {
        t.row(0).ends_with("reading… 1,499 files")
    });
    std::fs::write(dir.path().join("go"), "").expect("write");
    term.wait_for("the whole count", |t| t.row(0).ends_with(" 3,000 files"));
    type_raw(&mut term, "f2999");
    term.wait_for("the last file", |t| t.row(1).contains("dir2/f2999.txt"));
    type_raw(&mut term, "\r");
    term.wait_for("it opens", |t| {
        t.status().contains(" 3000/3000 ") && t.row(0).contains("dir2/f2999.txt")
    });
    term.wait_for("its diff", |t| t.contains("new 2999"));
    term.send("q");
    assert!(term.wait_exit().success());
}

/// Resident memory of process `pid`, in KB.
fn rss_kb(pid: &str) -> usize {
    if !cfg!(target_os = "linux") {
        let ps = Command::new("ps")
            .args(["-o", "rss=", "-p", pid])
            .output()
            .expect("ps");
        return String::from_utf8_lossy(&ps.stdout)
            .trim()
            .parse()
            .expect("ps rss");
    }
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("proc status");
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse().ok())
        .expect("VmRSS")
}

/// An endless writer while the picker is open: dffy reads on, with
/// patches on disk and memory bounded by entries, not input. Once the
/// picker closes, reading stops at the window again and the writer
/// blocks.
#[test]
fn an_open_picker_reads_an_endless_diff_and_esc_stops_it() {
    use std::fmt::Write as _;
    let dir = tempfile::tempdir().expect("tempdir");
    // 200 files of 4KB patches; written over and over, counting rounds.
    let mut chunk = String::new();
    for i in 0..200 {
        let _ = write!(
            chunk,
            "diff --git a/d/f{i}.c b/d/f{i}.c\n--- a/d/f{i}.c\n+++ b/d/f{i}.c\n@@ -1,40 +1,40 @@\n"
        );
        for l in 0..40 {
            let _ = write!(
                chunk,
                "-old line {l} of a file in an endless stream of patches\n\
                 +new line {l} of a file in an endless stream of patches\n"
            );
        }
    }
    std::fs::write(dir.path().join("chunk.diff"), &chunk).expect("write");
    let script = "i=0; while cat chunk.diff; do i=$((i+1)); echo $i > rounds; done | \
        sh -c 'echo $$ > pid; exec \"$DFFY\"'";
    let mut term = Term::spawn_sh(dir.path(), 100, 20, script);
    term.wait_for("the viewer", |t| t.row(0).starts_with("┌ Files"));
    let pid = std::fs::read_to_string(dir.path().join("pid")).expect("pid");
    let pid = pid.trim();
    let rounds = || {
        std::fs::read_to_string(dir.path().join("rounds"))
            .ok()
            .and_then(|r| r.trim().parse::<usize>().ok())
            .unwrap_or(0)
    };
    let before = rss_kb(pid);
    type_raw(&mut term, "o");
    // 100 rounds: 20,000 files and 66MB of patches.
    let start = Instant::now();
    while rounds() < 100 {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "{} rounds",
            rounds()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let open = rss_kb(pid);
    eprintln!("RSS {before} KB before, {open} KB after 20,000 files");
    assert!(open < before + 64 * 1024, "{before} KB, then {open} KB");
    type_raw(&mut term, "\x1b");
    term.wait_for("the picker to close", |t| !t.row(0).contains('>'));
    std::thread::sleep(Duration::from_millis(500));
    let stopped = rounds();
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(rounds(), stopped, "the writer blocks again");
    term.send("q");
    assert!(term.wait_exit().success());
}

/// A phone: `o` opens the picker, typing filters it, `enter` opens the
/// file picked; in the title, a tap on `›` goes to the next file and a
/// tap on the name opens the picker.
#[test]
fn a_phone_picks_files_by_name_and_by_tapping_the_title() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    for path in ["README.md", "fs/afs/inode.c", "kernel/sched/fair.c"] {
        std::fs::create_dir_all(d.join(path).parent().expect("parent")).expect("mkdir");
        std::fs::write(d.join(path), "one\ntwo\n").expect("write");
    }
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    for path in ["README.md", "fs/afs/inode.c", "kernel/sched/fair.c"] {
        std::fs::write(d.join(path), format!("one\n{path}\n")).expect("write");
    }
    let mut term = Term::spawn(d, 50, 15, &[]);
    term.wait_for("the first file", |t| t.contains("README.md"));
    assert!(term.status().ends_with("␣ commands"), "{}", term.status());
    term.send("o");
    assert!(term.row(0).contains('>'), "{}", term.vt.screen().contents());
    term.send("fair\r");
    assert!(term.row(0).contains(" 3/3 fair.c "), "{}", term.row(0));
    term.wait_for("its diff", |t| t.contains("kernel/sched/fair.c"));

    term.send("g");
    term.send("F");
    term.send("F");
    assert!(term.row(0).contains(" 1/3 README.md "), "{}", term.row(0));
    let column = |t: &Term, glyph: char| {
        t.row(0)
            .chars()
            .position(|c| c == glyph)
            .expect("on the title row")
    };
    let tap = |t: &mut Term, x: usize| t.send(&format!("\x1b[<0;{};1M\x1b[<0;{};1m", x + 1, x + 1));
    let next = column(&term, '›');
    tap(&mut term, next);
    assert!(term.row(0).contains(" 2/3 inode.c "), "{}", term.row(0));
    let name = column(&term, 'i');
    tap(&mut term, name);
    assert!(
        term.row(0).contains("3 files"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("\x1b");
    term.send("q");
    assert!(term.wait_exit().success());
}

#[test]
fn showing_a_commit_without_changes_shows_its_header() {
    let repo = repo();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "everything"]);
    git(
        repo.path(),
        &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "Nothing changed here",
        ],
    );
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_dffy"));
    cmd.args(["show", "HEAD"]);
    let mut term = Term::spawn_command(cmd, repo.path(), 100, 20, &[]);
    term.wait_for("the header", |t| t.contains("    Nothing changed here"));
    assert!(term.vt.screen().alternate_screen());
    assert!(
        term.contains("0 files +0 -0"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("q");
    assert!(term.wait_exit().success());
}

fn status_shows(term: &mut Term, text: &str) {
    term.wait_for(&format!("the status bar to show {text}"), |t| {
        t.status().contains(text)
    });
}

/// The command panel: `space` opens it, a row's key runs it and keeps it
/// open for another, `esc` closes it; `s` from it cycles the layout and
/// `i` the indicators; a remapped entry runs.
#[test]
fn the_command_panel_runs_entries_by_key() {
    let repo = repo();
    let config = config("keybind = leader:z=toggle_wrap\n");
    let mut term = Term::spawn(repo.path(), 160, 40, &[("XDG_CONFIG_HOME", &path(&config))]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    term.send(" ");
    assert!(
        term.contains("─ commands ─"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("\x1b");
    assert!(!term.contains("─ commands ─"), "esc closes it");
    // Auto (split at 160 columns), then split, unified, auto again.
    term.send(" s");
    status_shows(&mut term, "layout: split");
    term.send("s");
    status_shows(&mut term, "layout: unified");
    term.send("s");
    status_shows(&mut term, "layout: auto (split at this width)");
    assert!(term.contains("─ commands ─"), "toggles keep it open");
    term.send("i");
    assert!(
        term.contains("Indicators ‹ classic ›"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("z");
    assert!(
        term.contains("[x] Wrap long lines"),
        "{}",
        term.vt.screen().contents()
    );
    term.send("\x1b");
    term.send("q");
    assert!(term.wait_exit().success());
}

/// Whether any cell on screen has a 24-bit text color: the default theme
/// has none, the bundled palette themes nothing else.
fn truecolor(term: &Term) -> bool {
    let (rows, cols) = term.vt.screen().size();
    (0..rows.get()).any(|y| {
        (0..cols.get()).any(|x| {
            term.vt
                .screen()
                .cell(y, x)
                .is_some_and(|c| matches!(c.fgcolor(), vt100::Color::Rgb(..)))
        })
    })
}

/// The theme list: moving its cursor repaints the viewer in that theme,
/// `esc` brings back the one it opened on, `enter` keeps the one under the
/// cursor, and the next run starts with it.
#[test]
fn the_theme_list_tries_themes_and_the_choice_is_remembered() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 160, 40, &[]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    assert!(!truecolor(&term));
    // The theme row by filtering, then enter opens the list on it.
    term.send(" /theme\r\r");
    assert!(
        term.contains("─ themes ─"),
        "{}",
        term.vt.screen().contents()
    );
    // default, mono, then github-dark.
    term.send("\x1b[B\x1b[B");
    term.wait_for("github-dark's colors", truecolor);
    term.send("\x1b");
    term.wait_for("the default theme again", |t| !truecolor(t));
    assert!(!term.contains("─ themes ─"), "esc closes the list");
    term.send("\r");
    term.send("\x1b[B\x1b[B\r");
    term.wait_for("github-dark kept", truecolor);
    term.send("\x1b\x1b");
    term.send("q");
    assert!(term.wait_exit().success());
    let state = std::fs::read_to_string(repo.path().join(".git/dffy-test-state/dffy/state"))
        .expect("state file");
    assert!(state.contains("theme = github-dark\n"), "{state}");
    let mut term = Term::spawn(repo.path(), 160, 40, &[]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    term.wait_for("github-dark again", truecolor);
    term.send("q");
    assert!(term.wait_exit().success());
}

#[test]
fn s_cycles_split_unified_auto() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 160, 20, &[]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    for note in [
        "layout: split",
        "layout: unified",
        "layout: auto (split at this width)",
    ] {
        term.send("s");
        status_shows(&mut term, note);
    }
    term.send("q");
    assert!(term.wait_exit().success());
}

/// The first run shows the tip and notes it in `$XDG_STATE_HOME`; the
/// next run doesn't.
#[test]
fn the_first_run_tip_shows_once() {
    let repo = repo();
    let state = tempfile::tempdir().expect("tempdir");
    let state_home = path(&state);
    let env = [("XDG_STATE_HOME", state_home.as_str())];
    let mut term = Term::spawn(repo.path(), 100, 20, &env);
    status_shows(&mut term, "tip: space opens commands · ? lists every key");
    term.send("q");
    assert!(term.wait_exit().success());
    let saved = std::fs::read_to_string(state.path().join("dffy/state")).expect("state file");
    assert!(saved.contains("tip-shown = true"), "{saved}");
    let mut term = Term::spawn(repo.path(), 100, 20, &env);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    assert!(!term.status().contains("tip:"), "{}", term.status());
    term.send("q");
    assert!(term.wait_exit().success());
}

/// A layout picked in the viewer is the next run's; a flag beats it for
/// its run and isn't remembered.
#[test]
fn a_view_choice_is_remembered_and_a_flag_isnt() {
    let repo = repo();
    let launch = |args: &[&str]| {
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_dffy"));
        cmd.args(args);
        let mut term = Term::spawn_command(cmd, repo.path(), 160, 20, &[]);
        term.wait_for("the diffs", |t| t.contains(" +3 -3"));
        term
    };
    let mut term = launch(&[]);
    assert!(term.status().contains(" split "), "{}", term.status());
    term.send("ss");
    status_shows(&mut term, "layout: unified");
    term.send("q");
    assert!(term.wait_exit().success());
    let mut term = launch(&[]);
    assert!(term.status().contains(" unified "), "{}", term.status());
    term.send("q");
    assert!(term.wait_exit().success());
    let mut term = launch(&["--split"]);
    assert!(term.status().contains(" split "), "{}", term.status());
    term.send("q");
    assert!(term.wait_exit().success());
    let mut term = launch(&[]);
    assert!(
        term.status().contains(" unified "),
        "the flag wasn't remembered: {}",
        term.status()
    );
    term.send("q");
    assert!(term.wait_exit().success());
}

/// `/` in the diff: matches as typed, `enter` goes to the first, `n` on
/// into the next file with the count on the status bar, `esc` clears.
#[test]
fn slash_searches_the_diff() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 100, 20, &[]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    term.send("/More");
    assert!(term.status().starts_with("└ /More▏"), "{}", term.status());
    term.send("\r");
    status_shows(&mut term, "match 1/1");
    term.send("\x1b/line t");
    term.send("\r");
    term.wait_for("src/lib.rs", |t| t.row(0).contains("src/lib.rs"));
    status_shows(&mut term, "match 1/1");
    term.send("\x1b");
    status_shows(&mut term, "search cleared");
    term.send("q");
    assert!(term.wait_exit().success());
}

/// `/` in the tree filters it in place, fuzzily; `f` keeps to the files
/// it keeps; `esc` clears it. `o` still finds any file.
#[test]
fn slash_in_the_tree_filters_it() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 100, 20, &[]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    term.send("\t/lib\r");
    term.wait_for("the filter", |t| t.row(0).contains("Files · 1/2 · lib"));
    assert!(
        !term.contains("README.md"),
        "{}",
        term.vt.screen().contents()
    );
    assert!(term.row(0).contains("src/lib.rs"), "{}", term.row(0));
    term.send("\t");
    term.send("F");
    assert!(
        term.row(0).contains("src/lib.rs"),
        "f/F keep to the filter: {}",
        term.row(0)
    );
    term.send("\t\x1b");
    status_shows(&mut term, "tree filter cleared");
    assert!(term.contains("README.md"));
    term.send("oread\r");
    term.wait_for("README.md", |t| t.row(0).contains("README.md"));
    term.send("q");
    assert!(term.wait_exit().success());
}

/// A tap on `«` hides the tree, a tap on `»` shows it again.
#[test]
fn the_tree_buttons_hide_and_show_it() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 100, 20, &[]);
    term.wait_for("the tree", |t| t.row(0).starts_with("┌ Files"));
    assert!(term.row(19).starts_with("└«"), "{}", term.row(19));
    term.send("\x1b[<0;2;20M\x1b[<0;2;20m");
    assert!(!term.row(0).starts_with("┌ Files"), "{}", term.row(0));
    assert!(term.row(19).starts_with("└»"), "{}", term.row(19));
    term.send("\x1b[<0;2;20M\x1b[<0;2;20m");
    assert!(term.row(0).starts_with("┌ Files"), "{}", term.row(0));
    term.send("q");
    assert!(term.wait_exit().success());
}

/// `esc` with nothing to back out of doesn't quit; it says how to.
#[test]
fn esc_at_the_top_says_how_to_quit() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 100, 20, &[]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    term.send("\x1b");
    status_shows(&mut term, "press q to quit");
    assert!(
        term.child.try_wait().expect("wait").is_none(),
        "still running"
    );
    term.send("q");
    assert!(term.wait_exit().success());
}

/// With nothing unstaged, `--unstaged --watch` says what's staged, as the
/// printed message does, and that it's watching; it follows the index.
#[test]
fn an_empty_unstaged_view_says_what_is_staged_while_watching() {
    let repo = repo();
    let d = repo.path();
    git(d, &["add", "-A"]);
    let watching = Watching::new();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_dffy"));
    cmd.arg("--unstaged");
    let mut term = Term::spawn_command(cmd, d, 100, 20, &watching.env());
    term.wait_for("the empty text", |t| {
        t.contains("no unstaged changes (2 staged; run dffy to see them) — watching")
    });
    git(d, &["reset", "-q", "README.md"]);
    term.wait_for("the unstaged file", |t| t.contains("README.md"));
    git(d, &["add", "README.md"]);
    term.wait_for("the empty text again", |t| {
        t.contains("no unstaged changes (2 staged; run dffy to see them) — watching")
    });
    term.send("q");
    assert!(term.wait_exit().success());
}

/// A layout picked in the viewer is remembered for the viewer only:
/// printed output keeps the config's layout, so a script piping dffy sees
/// the same bytes whatever was last picked.
#[test]
fn a_layout_remembered_in_the_viewer_leaves_printed_output_alone() {
    let repo = repo();
    let d = repo.path();
    let mut term = Term::spawn(d, 160, 20, &[]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    // auto, then split.
    term.send("s");
    term.send("q");
    assert!(term.wait_exit().success());
    let state = d.join(".git/dffy-test-state");
    let saved = std::fs::read_to_string(state.join("dffy/state")).expect("state");
    assert!(saved.contains("view = split"), "{saved}");
    let out = Command::new(env!("CARGO_BIN_EXE_dffy"))
        .current_dir(d)
        .arg("--color=never")
        .env("XDG_CONFIG_HOME", d.join(".no-config"))
        .env("XDG_CACHE_HOME", d.join(".git/dffy-test-cache"))
        .env("XDG_STATE_HOME", &state)
        .env("COLUMNS", "160")
        .output()
        .expect("dffy runs");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("line one hundred"), "{text}");
    assert!(
        text.lines().all(|l| l.matches('│').count() <= 1),
        "split rows in printed output:\n{text}"
    );
    // The viewer itself starts split.
    let mut term = Term::spawn(d, 160, 20, &[]);
    term.wait_for("the split diff", |t| t.row(19).contains(" split "));
    term.send("q");
    assert!(term.wait_exit().success());
}

/// Whole files (`X`), ignored whitespace (the panel's `W`) and watching
/// (`w`) set in one run are on in the next. Flags beat them for their run
/// without being saved.
#[test]
fn whole_files_whitespace_and_watch_are_remembered() {
    let repo = repo();
    let d = repo.path();
    let watching = Watching::with_config("");
    let launch = |args: &[&str]| {
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_dffy"));
        cmd.args(args);
        let mut term = Term::spawn_command(cmd, d, 160, 40, &watching.env());
        term.wait_for("the diffs", |t| t.contains(" +3 -3"));
        term
    };
    let panel_row = |term: &mut Term, label: &str| {
        term.send(" ");
        let row = (0..40)
            .map(|y| term.row(y))
            .find(|r| r.contains(label))
            .unwrap_or_default();
        term.send("\x1b");
        row
    };
    let mut term = launch(&[]);
    term.send("X");
    status_shows(&mut term, " full");
    term.send(" W\x1b");
    term.send("w");
    status_shows(&mut term, " watch");
    term.send("q");
    assert!(term.wait_exit().success());
    let saved = std::fs::read_to_string(d.join(".git/dffy-test-state/dffy/state")).expect("state");
    for line in [
        "context = full",
        "ignore-all-space = true",
        "ignore-space-change = false",
        "watch = true",
    ] {
        assert!(saved.contains(line), "{line}: {saved}");
    }

    let mut term = launch(&[]);
    status_shows(&mut term, " full");
    status_shows(&mut term, " watch");
    let row = panel_row(&mut term, "Ignore whitespace");
    assert!(row.contains("[x] Ignore whitespace"), "{row}");
    term.send("q");
    assert!(term.wait_exit().success());

    let mut term = launch(&["-U3"]);
    assert!(!term.status().contains(" full"), "{}", term.status());
    term.send("q");
    assert!(term.wait_exit().success());
    let after = std::fs::read_to_string(d.join(".git/dffy-test-state/dffy/state")).expect("state");
    assert_eq!(after, saved, "the flag wasn't saved");
}

/// SIGTERM ends the viewer the way `q` does: a choice made just before,
/// still waiting out the write's debounce, is written.
#[test]
fn a_choice_is_written_on_sigterm() {
    let repo = repo();
    let d = repo.path();
    let mut term = Term::spawn(d, 160, 20, &[]);
    term.wait_for("the diffs", |t| t.contains(" +3 -3"));
    term.writer.write_all(b"X").expect("send X");
    term.writer.flush().expect("flush X");
    term.wait_for("whole files", |t| t.status().contains(" full"));
    let pid = term.child.process_id().expect("dffy's pid");
    let killed = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    term.wait_exit();
    let saved = std::fs::read_to_string(d.join(".git/dffy-test-state/dffy/state")).expect("state");
    assert!(saved.contains("context = full"), "{saved}");
}

/// Two folders outside git: the tree lists every changed file of both,
/// `f` and `n` move through them, and watching repaints an edit to a file
/// already shown as modified.
#[test]
fn two_folders_open_in_the_viewer_and_refresh_when_watched() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    let write = |path: &str, text: &str| {
        let full = d.join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        std::fs::write(full, text).expect("write");
    };
    write("old/a.txt", "alpha\nbeta\n");
    write("new/a.txt", "alpha\nBETA\n");
    write("old/gone.txt", "bye\n");
    write("old/sub/x.rs", "fn x() {}\n");
    write("new/sub/x.rs", "fn y() {}\n");
    write("new/sub/new.md", "# new\n");
    let runtime = tempfile::tempdir().expect("runtime");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_dffy"));
    cmd.args(["old", "new", "--watch"]);
    let mut term = Term::spawn_command(cmd, d, 120, 24, &[("XDG_RUNTIME_DIR", &path(&runtime))]);
    term.wait_for("the tree", |t| t.contains("Files · 4"));
    term.wait_for("the watch", |t| t.status().contains("watch"));
    assert!(term.status().contains("old/ → new/"), "{}", term.status());
    for name in ["a.txt", "gone.txt", "sub", "new.md", "x.rs"] {
        assert!(
            term.contains(name),
            "{name}: {}",
            term.vt.screen().contents()
        );
    }
    assert!(term.row(0).contains("a.txt"), "{}", term.row(0));
    term.send("f");
    term.wait_for("the next file", |t| t.row(0).contains("gone.txt"));
    term.send("n");
    term.wait_for("the next change", |t| t.row(0).contains("sub/new.md"));

    term.send("F");
    term.send("F");
    term.wait_for("back to the first file", |t| t.row(0).contains("a.txt"));
    write("new/a.txt", "alpha\nedited while watched\n");
    term.wait_for("the repaint", |t| t.contains("edited while watched"));
    term.send("q");
    assert!(term.wait_exit().success());
}

/// Two files: one entry named after the second, the two names in the
/// status line, and with watching, an edit to either file repainted.
#[test]
fn two_files_open_in_the_viewer_and_refresh_when_watched() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    std::fs::write(d.join("a.txt"), "alpha\nbeta\n").expect("write");
    std::fs::write(d.join("b.txt"), "alpha\nBETA\n").expect("write");
    let runtime = tempfile::tempdir().expect("runtime");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_dffy"));
    cmd.args(["a.txt", "b.txt", "--watch"]);
    let mut term = Term::spawn_command(cmd, d, 120, 24, &[("XDG_RUNTIME_DIR", &path(&runtime))]);
    term.wait_for("the diff", |t| t.contains("BETA"));
    term.wait_for("the watch", |t| t.status().contains("watch"));
    assert!(term.status().contains("a.txt → b.txt"), "{}", term.status());
    assert!(term.row(0).contains("b.txt"), "{}", term.row(0));
    std::fs::write(d.join("b.txt"), "alpha\nedited while watched\n").expect("write");
    term.wait_for("the new side's repaint", |t| {
        t.contains("edited while watched")
    });
    std::fs::write(d.join("a.txt"), "alpha\nold side edited\n").expect("write");
    term.wait_for("the old side's repaint", |t| t.contains("old side edited"));
    term.send("q");
    assert!(term.wait_exit().success());
}

/// One file whose diff is seven hunks with gaps between them, every ninth
/// line long enough to wrap in either layout.
fn scroll_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    let line = |i: usize, word: &str| {
        if i.is_multiple_of(9) {
            format!("{word} {i} {}\n", "words that wrap ".repeat(12))
        } else {
            format!("{word} {i}\n")
        }
    };
    let lines: Vec<String> = (1..=300).map(|i| line(i, "line")).collect();
    std::fs::write(d.join("lib.rs"), lines.concat()).expect("write");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    let mut changed = lines;
    for i in [12, 40, 41, 42, 90, 140, 141, 200, 260, 295] {
        changed[i - 1] = line(i * 3, "changed");
    }
    std::fs::write(d.join("lib.rs"), changed.concat()).expect("write");
    dir
}

/// Every cell's text, colors and attributes, an empty cell as a space: two
/// screens equal by this look the same, however their cells were written.
type Cells = Vec<(String, vt100::Color, vt100::Color, [bool; 5])>;

fn cells(term: &Term) -> Cells {
    let screen = term.vt.screen();
    let (rows, cols) = screen.size();
    (0..rows.get())
        .flat_map(|y| (0..cols.get()).map(move |x| (y, x)))
        .map(|(y, x)| {
            let c = screen.cell(y, x).expect("in range");
            let text = if c.has_contents() { c.contents() } else { " " };
            let attributes = [c.bold(), c.dim(), c.italic(), c.underline(), c.inverse()];
            (text.to_owned(), c.fgcolor(), c.bgcolor(), attributes)
        })
        .collect()
}

/// The keys scrolled through: by one, by the wheel's three, by half and
/// whole screens, both ways, at both ends of the file, then a change of
/// layout drawn over what scrolling left.
const SCROLLS: &[&str] = &[
    "j",
    "j",
    "j",
    "WHEEL_DOWN",
    "\x04",
    "j",
    "j",
    "\x06",
    "k",
    "k",
    "WHEEL_UP",
    "\x15",
    "k",
    "G",
    "j",
    "k",
    "k",
    "\x02",
    "g",
    "k",
    "j",
    "j",
    "s",
];

/// Scrolling lays out only the rows that come into view and has the
/// terminal move the rest, the tree's columns too when it's beside the
/// diff, whose cells are then sent again. The terminal must scroll the
/// diff's rows at least once, and after every key, the screen must be the
/// one a full redraw at the same place gives:
/// a second viewer gets each key, then `e` with an editor that exits at
/// once, after which it redraws on a cleared terminal.
///
/// Both first page through the whole file, so every hunk's emphasis is in
/// and no worker's result redraws a frame after the key's. Highlighting is
/// off: a parse over its time budget leaves a side plain in one viewer and
/// not the other.
fn scrolls_like_full_redraws(cols: u16, rows: u16, settings: &str, setup: &str) {
    let repo = scroll_repo();
    let config = config(&format!("highlight = false\n{settings}"));
    let env = [
        ("XDG_CONFIG_HOME", path(&config)),
        ("EDITOR", "true".to_owned()),
    ];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut scrolled = Term::spawn(repo.path(), cols, rows, &env);
    let mut redrawn = Term::spawn(repo.path(), cols, rows, &env);
    for term in [&mut scrolled, &mut redrawn] {
        term.wait_for("the diff", |t| t.contains("changed 36"));
        for _ in 0..8 {
            term.send("\x06");
        }
        term.send("g");
        term.send(setup);
    }
    let wheel = |button| format!("\x1b[<{button};{};10M", cols - 4);
    let before = scrolled.output().len();
    for (step, key) in SCROLLS.iter().enumerate() {
        let key = match *key {
            "WHEEL_DOWN" => wheel(65),
            "WHEEL_UP" => wheel(64),
            key => key.to_owned(),
        };
        scrolled.send(&key);
        let sent = redrawn.output().len();
        redrawn.send(&format!("{key}e"));
        redrawn.wait_for("the redraw after the editor", |t| {
            t.output()[sent..].windows(4).any(|w| w == b"\x1b[2J")
        });
        redrawn.settle();
        let start = Instant::now();
        loop {
            scrolled.update();
            redrawn.update();
            let (got, want) = (cells(&scrolled), cells(&redrawn));
            let Some(i) = (0..got.len()).find(|&i| got[i] != want[i]) else {
                break;
            };
            assert!(
                start.elapsed() < TIMEOUT,
                "after key {step} ({key:?}), the cell at row {}, column {} is {:?}, not \
                 {:?}\nscrolled:\n{}\nredrawn:\n{}",
                i / usize::from(cols),
                i % usize::from(cols),
                got[i],
                want[i],
                scrolled.vt.screen().contents(),
                redrawn.vt.screen().contents()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let region = format!("\x1b[2;{}r", rows - 1);
    assert!(
        scrolled.output()[before..]
            .windows(region.len())
            .any(|w| w == region.as_bytes()),
        "the terminal never scrolled the diff's rows"
    );
    for term in [&mut scrolled, &mut redrawn] {
        term.send("q");
        assert!(term.wait_exit().success());
    }
}

#[test]
fn scrolling_split_in_the_frame_matches_full_redraws() {
    scrolls_like_full_redraws(120, 30, "tree = hide\nview = split\n", "");
}

#[test]
fn scrolling_wrapped_unified_in_the_frame_matches_full_redraws() {
    scrolls_like_full_redraws(120, 30, "tree = hide\nview = unified\nwrap = true\n", "");
}

#[test]
fn scrolling_wrapped_split_beside_the_tree_matches_full_redraws() {
    scrolls_like_full_redraws(120, 30, "tree = show\nview = split\nwrap = true\n", "");
}

#[test]
fn scrolling_with_matches_lit_matches_full_redraws() {
    scrolls_like_full_redraws(80, 24, "tree = hide\nview = unified\n", "/line 1\r");
}

#[test]
fn scrolling_unified_beside_the_tree_with_matches_lit_matches_full_redraws() {
    scrolls_like_full_redraws(120, 30, "tree = show\nview = unified\n", "/line 1\r");
}

#[test]
fn option_arrows_go_between_files() {
    let repo = repo();
    let mut term = Term::spawn(repo.path(), 100, 12, &[]);
    term.wait_for("the diffs", |t| t.contains("More.") && t.contains(" +3 -3"));
    assert!(term.status().contains(" 1/2"), "{}", term.status());
    // macOS terminals send option+arrows as alt+f / alt+b ...
    term.send("\x1bf");
    term.wait_for("alt+f to the next file", |t| t.status().contains(" 2/2"));
    term.send("\x1bb");
    term.wait_for("alt+b to the previous file", |t| {
        t.status().contains(" 1/2")
    });
    // ... others as alt+right / alt+left.
    term.send("\x1b[1;3C");
    term.wait_for("alt+right to the next file", |t| {
        t.status().contains(" 2/2")
    });
    term.send("\x1b[1;3D");
    term.wait_for("alt+left to the previous file", |t| {
        t.status().contains(" 1/2")
    });
    term.send("q");
    term.wait_exit();
}

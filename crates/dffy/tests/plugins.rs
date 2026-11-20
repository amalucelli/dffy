//! Renderer plugins end to end: the built binary in a pseudo-terminal on a
//! repository with a changed README, with fake renderers (shell scripts on
//! PATH) and the real Markdown plugin.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::io::{Read, Write};
use std::num::NonZero;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Term {
    _master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    output: Arc<Mutex<Vec<u8>>>,
    vt: vt100::Parser,
    seen: usize,
    /// The viewer's dffyd's, so it never registers with the user's.
    _runtime: tempfile::TempDir,
}

impl Term {
    /// dffy in `repo` with the config `config`, its state under the repo's
    /// git directory and `bin` first on PATH.
    fn spawn(repo: &Path, config: &str, bin: &Path) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open a pty");
        let git = repo.join(".git");
        std::fs::create_dir_all(git.join("cfg/dffy")).expect("config dir");
        // Without the tree, the status bar has room for whole notes.
        std::fs::write(
            git.join("cfg/dffy/config"),
            format!("tree = hide\n{config}"),
        )
        .expect("config");
        let state = git.join("state/dffy/state");
        if !state.exists() {
            std::fs::create_dir_all(git.join("state/dffy")).expect("state dir");
            std::fs::write(&state, "version = 1\ntip-shown = true\n").expect("state");
        }
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_dffy"));
        cmd.cwd(repo);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("XDG_CONFIG_HOME", git.join("cfg"));
        cmd.env("XDG_CACHE_HOME", git.join("cache"));
        cmd.env("XDG_STATE_HOME", git.join("state"));
        let runtime = tempfile::tempdir().expect("tempdir");
        cmd.env("XDG_RUNTIME_DIR", runtime.path());
        cmd.env("DFFY_DAEMON_IDLE_SECS", "1");
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{path}", bin.display()));
        for name in [
            "NO_COLOR",
            "CLICOLOR_FORCE",
            "DFFY_PAGER",
            "COLUMNS",
            "TMUX",
        ] {
            cmd.env_remove(name);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn dffy");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().expect("lock").extend_from_slice(&buf[..n]);
            }
        });
        let writer = pair.master.take_writer().expect("pty writer");
        let (rows, cols) = (
            NonZero::new(24).expect("rows"),
            NonZero::new(100).expect("cols"),
        );
        Self {
            _master: pair.master,
            writer,
            child,
            output,
            vt: vt100::Parser::new(rows, cols, 0),
            seen: 0,
            _runtime: runtime,
        }
    }

    fn update(&mut self) -> bool {
        let out = self.output.lock().expect("lock").clone();
        let new = out.len() > self.seen;
        self.vt.process(&out[self.seen..]);
        self.seen = out.len();
        new
    }

    fn contains(&self, text: &str) -> bool {
        self.vt.screen().contents().contains(text)
    }

    fn status(&self) -> String {
        self.vt.screen().contents_between(23, 0, 23, 100)
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

    fn send(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).expect("send keys");
        self.writer.flush().expect("flush keys");
    }

    /// The cell where `text` starts.
    fn cell(&self, text: &str) -> Option<vt100::Cell> {
        let screen = self.vt.screen();
        (0..24).find_map(|y| {
            let row = screen.contents_between(y, 0, y, 100);
            let x = row.find(text)?;
            let column = u16::try_from(row[..x].chars().count()).ok()?;
            screen.cell(y, column).cloned()
        })
    }

    fn bg(&self, text: &str) -> Option<vt100::Color> {
        self.cell(text).map(|c| c.bgcolor())
    }

    fn quit(&mut self) {
        self.send("q");
        let start = Instant::now();
        while self.child.try_wait().expect("wait").is_none() {
            assert!(start.elapsed() < TIMEOUT, "dffy didn't exit");
            std::thread::sleep(Duration::from_millis(10));
        }
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

/// A repository whose README.md changed from `old` to `new`, uncommitted.
fn repo(old: &str, new: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    std::fs::write(d.join("README.md"), old).expect("write");
    std::fs::write(d.join("notes.txt"), "one\n").expect("write");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    std::fs::write(d.join("README.md"), new).expect("write");
    std::fs::write(d.join("notes.txt"), "two\n").expect("write");
    dir
}

const OLD: &str =
    "# Title\n\nThe quick brown fox jumps.\n\nThis paragraph goes away.\n\nShared tail.\n";
const NEW: &str = "# Title\n\nThe quick red fox jumps.\n\nShared tail.\n\nA brand new paragraph.\n";

/// What the fake renderer answers for [`OLD`] and [`NEW`]: their blocks
/// in capitals, so the screen tells them from the source lines.
const ANSWER: &str = r#"{"version":1,
"old":[{"kind":"heading","source":[0,1],"lines":[[{"text":"TITLE","style":["heading"],"level":1}]]},
{"kind":"paragraph","source":[2,3],"lines":[["THE QUICK BROWN FOX JUMPS."]]},
{"kind":"paragraph","source":[4,5],"lines":[["THIS PARAGRAPH GOES AWAY."]]},
{"kind":"paragraph","source":[6,7],"lines":[["SHARED TAIL."]]}],
"new":[{"kind":"heading","source":[0,1],"lines":[[{"text":"TITLE","style":["heading"],"level":1}]]},
{"kind":"paragraph","source":[2,3],"lines":[["THE QUICK RED FOX JUMPS."]]},
{"kind":"paragraph","source":[4,5],"lines":[["SHARED TAIL."]]},
{"kind":"paragraph","source":[6,7],"lines":[["A BRAND NEW PARAGRAPH."]]}]}"#;

/// A directory holding the test renderer as `dffy-render-<name>`, doing
/// what its name says (see the script), with [`ANSWER`] beside it.
fn renderer(name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("dffy-render-{name}"));
    std::fs::write(&path, include_str!("fixtures/stubs/renderer.sh")).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::write(dir.path().join("answer.json"), ANSWER).expect("write");
    dir
}

fn rgb(color: Option<vt100::Color>) -> (u8, u8, u8) {
    match color {
        Some(vt100::Color::Rgb(r, g, b)) => (r, g, b),
        other => panic!("not a truecolor tint: {other:?}"),
    }
}

#[test]
fn the_rendered_view_shows_blocks_diffed_with_tints_and_word_emphasis() {
    let repo = repo(OLD, NEW);
    let bin = renderer("answers");
    let mut term = Term::spawn(repo.path(), "render = md:answers\n", bin.path());
    term.wait_for("the line diff", |t| t.contains("A brand new paragraph."));
    term.send("r");
    term.wait_for("the rendered view", |t| {
        t.contains("A BRAND NEW PARAGRAPH.") && t.status().contains("rendered")
    });
    assert!(!term.contains("# Title"), "{}", term.vt.screen().contents());
    let (added, removed) = (
        rgb(term.bg("A BRAND NEW")),
        rgb(term.bg("THIS PARAGRAPH GOES")),
    );
    assert!(added.1 > added.0, "added is green: {added:?}");
    assert!(removed.0 > removed.1, "removed is red: {removed:?}");
    let (line, word) = (rgb(term.bg("THE QUICK RED")), rgb(term.bg("RED FOX")));
    assert_ne!(line, added, "an edited paragraph is modified, not added");
    assert_ne!(line, word, "its changed word is emphasized");
    assert_eq!(term.bg("TITLE"), Some(vt100::Color::Default));
    let title = term.cell("TITLE").expect("the heading");
    assert!(
        title.bold() && title.underline(),
        "a level 1 heading is bold and underlined"
    );
    term.quit();
}

#[test]
fn r_toggles_the_rendered_view_and_is_remembered() {
    let repo = repo(OLD, NEW);
    let bin = renderer("answers");
    let mut term = Term::spawn(repo.path(), "render = md:answers\n", bin.path());
    term.wait_for("the line diff", |t| t.contains("# Title"));
    term.send("r");
    term.wait_for("rendered", |t| {
        t.contains("TITLE") && !t.contains("# Title")
    });
    term.send("r");
    term.wait_for("the lines", |t| {
        t.contains("# Title") && !t.status().contains("rendered")
    });
    term.send("r");
    term.wait_for("rendered again", |t| t.contains("SHARED TAIL."));
    term.quit();
    let state = std::fs::read_to_string(repo.path().join(".git/state/dffy/state")).expect("state");
    assert!(state.contains("rendered = true"), "{state}");

    let mut term = Term::spawn(repo.path(), "render = md:answers\n", bin.path());
    term.wait_for("rendered from the start", |t| {
        t.contains("A BRAND NEW PARAGRAPH.")
    });
    term.quit();
}

/// The failed renderer isn't asked again for that file.
#[test]
fn a_slow_or_broken_renderer_falls_back_to_the_lines_with_a_note() {
    for (name, note) in [("slow", "took over 500ms"), ("broken", "bad answer")] {
        let repo = repo(OLD, NEW);
        let bin = renderer(name);
        let runs = bin.path().join("runs");
        let mut term = Term::spawn(repo.path(), &format!("render = md:{name}\n"), bin.path());
        term.wait_for("the line diff", |t| t.contains("# Title"));
        term.send("r");
        term.wait_for(note, |t| t.status().contains(note));
        assert!(term.contains("# Title"));
        for key in ["j", "k", "f", "F"] {
            term.send(key);
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_millis(300));
        let count = std::fs::read_to_string(&runs)
            .expect("runs")
            .lines()
            .count();
        assert_eq!(count, 1, "{note}: asked again");
        term.quit();
    }
}

#[test]
fn moving_away_cancels_a_running_renderer() {
    let repo = repo(OLD, NEW);
    let bin = renderer("late");
    let runs = bin.path().join("runs");
    let mut term = Term::spawn(repo.path(), "render = md:late\n", bin.path());
    term.wait_for("the line diff", |t| t.contains("# Title"));
    term.send("r");
    term.wait_for("the renderer", |_| runs.exists());
    term.send("f");
    term.wait_for("the next file", |t| {
        t.contains("notes.txt") && t.contains("two")
    });
    std::thread::sleep(Duration::from_millis(3500));
    term.update();
    assert_eq!(std::fs::read_to_string(&runs).expect("runs"), "run\n");
    assert!(!term.status().contains("took over"), "{}", term.status());
    term.quit();
}

/// Whether process `pid` is running: neither gone nor a zombie.
fn alive(pid: &str) -> bool {
    Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .is_ok_and(|out| {
            let stat = String::from_utf8_lossy(&out.stdout);
            out.status.success() && !stat.trim().is_empty() && !stat.trim().starts_with('Z')
        })
}

/// Kills what the renderer started too, not only the renderer.
#[test]
fn quitting_stops_a_running_renderer() {
    let repo = repo(OLD, NEW);
    let bin = renderer("forks");
    let pids = bin.path().join("pids");
    let mut term = Term::spawn(repo.path(), "render = md:forks\n", bin.path());
    term.wait_for("the line diff", |t| t.contains("# Title"));
    term.send("r");
    term.wait_for("the renderer", |_| pids.exists());
    term.quit();
    let pids = std::fs::read_to_string(&pids).expect("pids");
    let pids: Vec<&str> = pids.split_whitespace().collect();
    assert_eq!(pids.len(), 2, "{pids:?}");
    let start = Instant::now();
    while pids.iter().any(|p| alive(p)) && start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let left: Vec<_> = pids.iter().filter(|p| alive(p)).collect();
    for pid in &left {
        Command::new("kill").args(["-9", pid]).status().ok();
    }
    assert!(left.is_empty(), "still running after dffy quit: {left:?}");
}

#[test]
fn a_missing_renderer_gives_a_hint() {
    let repo = repo(OLD, NEW);
    let empty = tempfile::tempdir().expect("tempdir");
    let mut term = Term::spawn(repo.path(), "", empty.path());
    term.wait_for("the line diff", |t| t.contains("# Title"));
    term.send("r");
    term.wait_for("the hint", |t| {
        t.status()
            .contains("no renderer for .md (see docs/plugins.md)")
    });
    term.quit();

    let mut term = Term::spawn(repo.path(), "render = md:nothere\n", empty.path());
    term.wait_for("the line diff", |t| t.contains("# Title"));
    term.send("r");
    term.wait_for("the hint", |t| {
        t.status()
            .contains("dffy-render-nothere isn't on PATH (see docs/plugins.md)")
    });
    assert!(!term.status().contains("rendered"));
    term.quit();
}

#[test]
fn no_renderer_runs_until_r_is_pressed() {
    let repo = repo(OLD, NEW);
    let bin = renderer("answers");
    let runs = bin.path().join("runs");
    let mut term = Term::spawn(repo.path(), "render = md:answers\n", bin.path());
    term.wait_for("the line diff", |t| t.contains("# Title"));
    for key in ["f", "F", "n", "N", "j", "k"] {
        term.send(key);
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(!runs.exists(), "the renderer ran unasked");
    term.send("r");
    term.wait_for("the rendered view", |t| t.contains("SHARED TAIL."));
    assert!(runs.exists());
    term.quit();
}

/// The real Markdown plugin, built with the workspace, on a README edited
/// the way READMEs are: a heading, a list item, a table cell, a code line.
#[test]
fn the_markdown_plugin_renders_a_readme_diff() {
    let plugin = Path::new(env!("CARGO_BIN_EXE_dffy")).with_file_name("dffy-render-markdown");
    if !plugin.exists() {
        eprintln!(
            "skipped: {} isn't built; cargo build --workspace builds it",
            plugin.display()
        );
        return;
    }
    let bin = tempfile::tempdir().expect("tempdir");
    std::os::unix::fs::symlink(&plugin, bin.path().join("dffy-render-markdown")).expect("link");
    let old = "# dffy\n\nA fast diff viewer.\n\n## Install\n\n- brew install dffy\n- cargo install dffy\n\n\
               | Key | Action |\n|-----|--------|\n| q | quit |\n| n | next change |\n\n\
               ```sh\ndffy main..HEAD\n```\n";
    let new = "# dffy\n\nA fast terminal diff viewer.\n\n## Installing\n\n- brew install dffy\n- cargo install dffy\n\
               - nix profile install dffy\n\n| Key | Action |\n|-----|--------|\n| q | quit |\n| n | next change, on into the next file |\n\n\
               ```sh\ndffy main..HEAD --watch\n```\n";
    let repo = repo(old, new);
    let mut term = Term::spawn(repo.path(), "render = md:markdown\n", bin.path());
    term.wait_for("the line diff", |t| t.contains("## Install"));
    term.send("r");
    term.wait_for("the rendered view", |t| {
        t.contains("• nix profile install dffy") && t.contains("Installing") && t.contains("┌")
    });
    assert!(
        !term.contains("## Install"),
        "{}",
        term.vt.screen().contents()
    );
    let added = rgb(term.bg("nix profile"));
    assert!(added.1 > added.0, "the new list item is added: {added:?}");
    assert_eq!(
        term.bg("cargo install"),
        Some(vt100::Color::Default),
        "the item that was last before is unchanged"
    );
    assert!(term.status().contains("rendered"));
    term.send("G");
    term.wait_for("the code block", |t| t.contains("dffy main..HEAD --watch"));
    term.quit();
}

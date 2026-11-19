use std::ops::ControlFlow;
use std::path::Path;

use dffy_core::diff::{Options, Text, Whitespace, hunks};
use dffy_core::document::{self, Counts};
use dffy_core::git::{ChangeSet, FileDiff, Mode, Repo, Side};
use dffy_core::view::Attributes;

use super::draw::note;
use super::*;
use crate::plugin::review::{Draft, Listed, Plan, Push, Reason, Request, Said, Session};
use crate::review::{self, Change, Op, Target};
use crate::theme::{Ink, Role, Style};
use crate::tui::picker;

/// `fd` prepared as a worker does without highlighting.
fn loaded(fd: FileDiff) -> File {
    File::prepare(crate::grammars(), None).file(fd, Whitespace::Exact, false)
}

fn diff(path: &str) -> FileDiff {
    let side = Some(Side::Worktree {
        mode: Mode::File,
        id: None,
    });
    let old = (1..=60)
        .map(|i| format!("line {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    let new = old
        .replace("line 4\n", "line four\n")
        .replace("line 50\n", "line fifty\n");
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    FileDiff {
        change: FileChange {
            path: path.into(),
            status: Status::Modified,
            old: side,
            new: side,
            staging: None,
        },
        hunks: hunks(&old, &new, &Options::default()),
        old,
        new,
        binary: false,
        too_large: false,
        note: None,
    }
}

/// A `width` × `height` viewer of two files, the first diffed.
fn app(width: usize, height: usize) -> App {
    let first = diff("src/some/deeply/nested/module/file.rs");
    app_of(&Config::default(), "working tree", first, (width, height))
}

/// A viewer of `first`, diffed, and a second file, comparing what
/// `description` says.
fn app_of(config: &Config, description: &str, first: FileDiff, size: (usize, usize)) -> App {
    app_with(config, description, loaded(first), size)
}

/// [`app_of`] with the first file prepared already.
fn app_with(config: &Config, description: &str, loaded: File, size: (usize, usize)) -> App {
    let mut app = App::new(
        config,
        Theme::default_theme(),
        description.to_owned(),
        Some(loaded.diff().change.clone()),
        size,
        Indicators::Classic,
    );
    app.event(Event::Enumerated(diff("src/zz.rs").change));
    app.event(Event::EnumerationDone(None));
    app.event(Event::Diffed(Done::Diff {
        placed: None,
        file: 0,
        version: app.version(0),
        result: Ok(Box::new(loaded)),
    }));
    app
}

/// Draws, placing comments as the pool would after a draw asks for it.
fn screen(app: &mut App) -> String {
    app.draw();
    if place(app) {
        app.draw();
    }
    app.screen.text()
}

/// `fd` diffed for `file` as the pool answers, with the comments as
/// they are when it comes placed on it.
fn diffed(app: &App, file: usize, fd: FileDiff) -> Done {
    let loaded = loaded(fd);
    let annotations = (app.comments.as_ref()).map(Comments::annotations);
    Done::Diff {
        file,
        version: app.version(file),
        placed: annotations.and_then(|a| crate::tui::worker::place_on(a, &loaded, false)),
        result: Ok(Box::new(loaded)),
    }
}

/// Runs the placements the viewer asks for, as the pool would. Returns
/// whether there were any.
fn place(app: &mut App) -> bool {
    let jobs: Vec<PlaceJob> = (app.jobs().into_iter())
        .filter_map(|job| match job {
            Job::Place(job) => Some(job),
            _ => None,
        })
        .collect();
    let any = !jobs.is_empty();
    for job in jobs {
        app.event(Event::Diffed(job.run()));
    }
    any
}

fn key(app: &mut App, c: char) {
    app.event(Event::Input(Input::Key(Key::Char(c), Mods::NONE)));
}

/// A new file of `lines` lines: one whole-file hunk.
fn added(lines: usize) -> FileDiff {
    let new = (1..=lines)
        .map(|i| format!("line {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    let (old, new) = (Text::new(Vec::new()), Text::new(new.into()));
    FileDiff {
        change: FileChange {
            path: "notes.txt".into(),
            status: Status::Added,
            old: None,
            new: Some(Side::Worktree {
                mode: Mode::File,
                id: None,
            }),
            staging: None,
        },
        hunks: hunks(&old, &new, &Options::default()),
        old,
        new,
        binary: false,
        too_large: false,
        note: None,
    }
}

/// Each language in the list goes to the pool to compile ahead once, as
/// its first file is listed. With highlighting off none do, until it's
/// turned on.
#[test]
fn each_listed_language_is_compiled_ahead_once() {
    let names = |languages: Vec<Language>| -> Vec<String> {
        languages.iter().map(|l| l.name().to_owned()).collect()
    };
    let viewer = |config: &Config, first: Option<FileChange>| {
        App::new(
            config,
            Theme::default_theme(),
            "working tree".to_owned(),
            first,
            (100, 20),
            Indicators::Classic,
        )
    };
    let mut app = viewer(&Config::default(), Some(diff("src/main.rs").change));
    for path in [
        "src/lib.rs",
        "App.swift",
        "README",
        "Cargo.lock",
        "Sources/B.swift",
    ] {
        app.event(Event::Enumerated(diff(path).change));
    }
    assert_eq!(names(app.take_languages()), ["rust", "swift", "toml"]);
    app.event(Event::Enumerated(diff("src/c.rs").change));
    app.event(Event::Enumerated(diff("b.kt").change));
    app.event(Event::EnumerationDone(None));
    assert_eq!(names(app.take_languages()), ["kotlin"]);
    assert_eq!(names(app.take_languages()), [""; 0]);

    let off = Config {
        highlight: false,
        ..Config::default()
    };
    let mut app = viewer(&off, None);
    app.event(Event::Enumerated(diff("src/lib.rs").change));
    assert_eq!(names(app.take_languages()), [""; 0]);
    app.rediff(Act::ToggleHighlight);
    assert_eq!(names(app.take_languages()), ["rust"]);
}

/// The file is diffed again while emphasis for the diff still shown is
/// being computed, and the new diff lands first: that emphasis was
/// computed for the old hunk and must not be drawn with the new one.
/// The pool held back the new hunk's job while the old one ran, so
/// dropping the old result draws again to ask for it.
#[test]
fn emphasis_for_a_diff_since_replaced_is_dropped() {
    let mut app = app_of(&Config::default(), "working tree", added(46), (187, 80));
    screen(&mut app);
    // Diffing again: the old diff stays shown until the new one lands.
    key(&mut app, ' ');
    key(&mut app, 'I');
    // The panel stays open after a toggle, over the rows looked at.
    press(&mut app, Key::Esc);
    screen(&mut app);
    let jobs = app.jobs();
    let Some(Job::Emphasis {
        file,
        version,
        hunk,
        diff,
        ..
    }) = jobs.into_iter().find(|j| matches!(j, Job::Emphasis { .. }))
    else {
        panic!("emphasis for the hunk shown");
    };
    assert_eq!(diff.new.len(), 46);
    app.event(Event::Diffed(Done::Diff {
        placed: None,
        file,
        version,
        result: Ok(Box::new(loaded(added(60)))),
    }));
    assert!(screen(&mut app).contains("line 60"));
    app.event(Event::Diffed(Done::Emphasis {
        file,
        version,
        hunk,
        emphasis: dffy_core::diff::emphasize(&diff.old, &diff.new, &diff.hunks[hunk]),
        syntax: None,
        diff,
    }));
    assert!(app.draw().is_some());
    let current = app.doc.document.diff_of(file).map(Arc::clone);
    assert!(app.jobs().iter().any(|j| matches!(
        j,
        Job::Emphasis { diff, .. } if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, diff))
    )));
}

/// A watch refresh voids the work of the files it touched and no
/// other: under constant writes to one file, another's emphasis still
/// lands, and a diff of the written file taken before the write is
/// never shown as current.
#[test]
fn a_refresh_of_one_file_keeps_the_work_for_the_others() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(d)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "a@example.com")
            .env("GIT_COMMITTER_NAME", "A")
            .env("GIT_COMMITTER_EMAIL", "a@example.com")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(d.join("a.txt"), "a\n").expect("write");
    std::fs::write(d.join("b.txt"), "one\ntwo\n").expect("write");
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "x"]);
    std::fs::write(d.join("a.txt"), "a2\n").expect("write");
    std::fs::write(d.join("b.txt"), "one\nTWO\n").expect("write");

    let repo = Repo::discover(d).expect("repo");
    let spec = repo.resolve(&[], false, false).expect("spec");
    let mut set = ChangeSet::new(repo.clone(), spec, Revisions::default(), &[], false);
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        None,
        (100, 20),
        Indicators::Classic,
    );
    set.enumerate(|change| {
        app.event(Event::Enumerated(change));
        ControlFlow::Continue(())
    })
    .expect("enumerate");
    app.event(Event::EnumerationDone(None));
    let diffed = |app: &App, file: usize| {
        let change = app.doc.document.files()[file].change().clone();
        let fd = repo.file_diff(change, &Options::default()).expect("diff");
        diffed(app, file, fd)
    };
    for file in [0, 1] {
        let done = diffed(&app, file);
        app.event(Event::Diffed(done));
    }
    key(&mut app, 'f');
    assert_eq!(
        app.doc.document.files()[app.doc.document.pos.file]
            .change()
            .path,
        "b.txt"
    );
    app.draw();
    let emphasis = app
        .jobs()
        .into_iter()
        .find_map(|job| match job {
            Job::Emphasis {
                file,
                version,
                hunk,
                diff,
                ..
            } => Some(Done::Emphasis {
                file,
                version,
                hunk,
                emphasis: dffy_core::diff::emphasize(&diff.old, &diff.new, &diff.hunks[hunk]),
                syntax: None,
                diff,
            }),
            _ => None,
        })
        .expect("emphasis for b.txt's hunk");
    let before_the_write = diffed(&app, 0);

    std::fs::write(d.join("a.txt"), "a3\n").expect("write");
    let update = set
        .refresh(&["a.txt".into()])
        .expect("refresh")
        .expect("changed");
    app.event(Event::Refreshed(update));
    app.event(Event::Diffed(emphasis));
    app.event(Event::Diffed(before_the_write));

    assert!(app.draw().is_some());
    assert!(
        !app.jobs().iter().any(|j| matches!(j, Job::Emphasis { .. })),
        "b.txt's emphasis was kept"
    );
    assert!(
        app.doc.document.files()[0].wants_diff(),
        "a.txt's diff from before the write isn't taken"
    );
}

#[test]
fn a_phone_sized_screen_compacts_the_status_line_and_overlays_the_tree() {
    let mut app = app(40, 12);
    insta::assert_snapshot!("narrow", screen(&mut app));
    app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
    insta::assert_snapshot!("narrow_tree", screen(&mut app));
    // Picking a file closes the overlay.
    key(&mut app, 'j');
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    let shown = screen(&mut app);
    assert!(!shown.contains("┌ Files"), "{shown}");
    assert!(
        shown.lines().next().is_some_and(|l| l.contains("2/2")),
        "{shown}"
    );
}

/// The screen with each run of the status chip's cells in `[…]`.
fn chip_markup(app: &mut App) -> String {
    app.draw();
    let tag = app.theme.style(Role::StatusTag);
    let grid = app.screen.shown_grid();
    (0..grid.height())
        .map(|y| {
            let mut line = String::new();
            let mut on = false;
            for cell in grid.row(y) {
                let now = cell.style() == tag;
                if now != on {
                    line.push(if now { '[' } else { ']' });
                    on = now;
                }
                line.push_str(cell.text());
            }
            line.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One frame around the tree and the diff: the top border holds the
/// tree's totals, the file's title and the hunk the view is in; the
/// bottom border is the status bar. The divider joins both with `┬`
/// and `┴`.
#[test]
fn the_frame_holds_the_title_context_and_status() {
    let mut app = app(120, 18);
    insta::assert_snapshot!("frame_wide", chip_markup(&mut app));
    // Without the tree, the frame goes round the diff alone; scrolled
    // into the second hunk, the context follows.
    key(&mut app, 't');
    key(&mut app, 'n');
    key(&mut app, 'n');
    insta::assert_snapshot!("frame_no_tree", chip_markup(&mut app));
}

/// A file changed inside a function named on its first line, the
/// heading of its one hunk.
fn heading_diff() -> FileDiff {
    let old: String = std::iter::once("fn render_the_frame_and_its_status_bar() {\n".to_owned())
        .chain((1..=40).map(|i| format!("    let line_{i} = {i};\n")))
        .chain(["}\n".to_owned()])
        .collect();
    let new = old.replace("let line_20 = 20;", "let line_20 = 21;");
    let side = Some(Side::Worktree {
        mode: Mode::File,
        id: None,
    });
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    FileDiff {
        change: FileChange {
            path: "src/some/deeply/nested/module/file.rs".into(),
            status: Status::Modified,
            old: side,
            new: side,
            staging: None,
        },
        hunks: hunks(&old, &new, &Options::default()),
        old,
        new,
        binary: false,
        too_large: false,
        note: None,
    }
}

/// Short of room, the context shrinks first, then the directory, cut
/// from the left; the name and counts stay.
#[test]
fn the_title_gives_up_the_context_then_the_directory() {
    let titles: Vec<String> = [130, 100, 72, 60, 48]
        .into_iter()
        .map(|diff_width| {
            // The diff's part of the top border, the tree taking the
            // rest of the screen; in the hunk, its whole text shown.
            let mut app = app_of(&Config::default(), "uncommitted", heading_diff(), (160, 18));
            app.tree_width = Some(160 - diff_width);
            app.relayout();
            key(&mut app, 'X');
            key(&mut app, 'n');
            let shown = screen(&mut app);
            let top = shown.lines().next().unwrap_or_default();
            top.chars().skip(160 - diff_width - 1).collect::<String>()
        })
        .collect();
    insta::assert_snapshot!(titles.join("\n"), @r"
        ┬─ modified · src/some/deeply/nested/module/file.rs · +1 -1 ────────────────────── @@ fn render_the_frame_and_its_status_bar() { ─┐
        ┬─ modified · src/some/deeply/nested/module/file.rs · +1 -1 ─ @@ fn render_the_frame_and_its_stat… ─┐
        ┬─ modified · src/some/deeply/nested/module/file.rs · +1 -1 ────────────┐
        ┬─ modified · …c/some/deeply/nested/module/file.rs · +1 -1 ─┐
        ┬─ modified · …y/nested/module/file.rs · +1 -1 ─┐
        ");
}

/// A change inside `impl Error › fn fatal` whose hunk starts above
/// `impl Error`, so git's heading for it is the function before.
fn scope_diff() -> FileDiff {
    let old: String =
        ["pub(crate) fn grammars() {\n    let a = 1;\n}\n\nimpl Error {\n".to_owned()]
            .into_iter()
            .chain(["    pub(crate) fn fatal(&self) -> bool {\n".to_owned()])
            .chain((1..=20).map(|i| format!("        let line_{i} = {i};\n")))
            .chain(["        true\n    }\n}\n".to_owned()])
            .collect();
    let new = old
        .replace("let line_2 = 2;", "let line_2 = 22;")
        .replace("let line_8 = 8;", "let line_8 = 88;")
        .replace("let line_14 = 14;", "let line_14 = 144;");
    let mut fd = heading_diff();
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    fd.change.path = "src/error.rs".into();
    fd.hunks = hunks(&old, &new, &Options::default());
    (fd.old, fd.new) = (old, new);
    fd
}

/// Where the language has a scope query, the title says which
/// definitions the top line is in, the innermost kept when it's cut;
/// without highlighting, it's the hunk's heading as git finds it.
#[test]
fn the_title_names_the_definitions_around_the_top_line() {
    let titles: Vec<String> = [(130, true), (58, true), (130, false)]
        .into_iter()
        .map(|(diff_width, highlight)| {
            let prepare = File::prepare(crate::grammars(), None);
            let file = prepare.file(scope_diff(), Whitespace::Exact, highlight);
            if let Some(slot) = file.syntax() {
                slot.get_or_init(|| prepare.syntax(file.diff(), false));
            }
            let mut app = app_with(&Config::default(), "uncommitted", file, (160, 18));
            app.tree_width = Some(160 - diff_width);
            app.relayout();
            key(&mut app, 'n');
            for _ in 0..4 {
                key(&mut app, 'j');
            }
            let shown = screen(&mut app);
            let top = shown.lines().next().unwrap_or_default();
            top.chars().skip(160 - diff_width - 1).collect::<String>()
        })
        .collect();
    insta::assert_snapshot!(titles.join("\n"), @r"
        ┬─ modified · src/error.rs · +3 -3 ─────────────────────────────────────────────────────────────────────── impl Error › fn fatal ─┐
        ┬─ modified · src/error.rs · +3 -3 ─ …l Error › fn fatal ─┐
        ┬─ modified · src/error.rs · +3 -3 ─────────────────────────────────────────────────────────────── @@ pub(crate) fn grammars() { ─┐
        ");
}

#[test]
fn a_note_replaces_the_mode_words() {
    let mut app = app(120, 18);
    key(&mut app, 'W');
    let shown = screen(&mut app);
    let status = shown.lines().last().unwrap_or_default();
    assert!(status.contains("wrap"), "{status}");
    app.message("copied src/some/deeply/nested/module/file.rs");
    insta::assert_snapshot!(screen(&mut app).lines().last().unwrap_or_default(), @"└«─  working tree  ─────────────┴─ 1/2 · 1/2 hunk  copied src/some/deeply/nested/module/file.rs ────── space commands ─┘");
}

/// Under 16 rows there's no frame: a title row, then code, then the
/// status row.
#[test]
fn a_short_screen_has_a_title_row_and_a_status_row() {
    let mut app = app(50, 15);
    insta::assert_snapshot!("short", chip_markup(&mut app));
}

/// `status-bar = top` puts the status in the top border after the
/// title, and in the toggle's state remembered.
#[test]
fn the_status_bar_can_go_on_top() {
    let mut config = Config::default();
    config.apply_source("status-bar = top\n", &mut Vec::new());
    let mut app = app_of(&config, "v6.0..v6.1", heading_diff(), (160, 16));
    key(&mut app, 'X');
    key(&mut app, 'n');
    insta::assert_snapshot!("status_top", chip_markup(&mut app));
    key(&mut app, ' ');
    key(&mut app, 'B');
    assert_eq!(app.status_bar, StatusBar::Bottom);
    assert_eq!(
        app.take_remembered(true),
        [
            ("context", "full".to_owned()),
            ("status-bar", "bottom".to_owned())
        ]
    );
}

/// At 40 columns a long chip keeps its own cells: what doesn't fit
/// after it goes, rather than drawing over it.
#[test]
fn a_long_chip_on_a_phone_is_never_overdrawn() {
    let mut app = app_of(
        &Config::default(),
        "origin/main...HEAD",
        diff("src/a.rs"),
        (40, 12),
    );
    let shown = screen(&mut app);
    let status = shown.lines().last().unwrap_or_default();
    assert!(status.contains(" origin/main...HEAD "), "{status}");
    assert!(status.chars().count() <= 40, "{status}");
}

/// The picker covers the tree, so the frame's edges go straight across
/// while it's open.
#[test]
fn the_frame_loses_the_tree_under_the_picker() {
    let mut app = app(200, 60);
    key(&mut app, 'o');
    let shown = screen(&mut app);
    let status = shown.lines().last().unwrap_or_default();
    assert!(status.starts_with("└──"), "{status}");
    assert!(!status.contains('┴'), "{status}");
}

/// A split remembered from an earlier run shows unified on a phone and
/// stays the saved choice; split asked for in this run (a flag, the
/// config) splits there, down to the per-hunk fallback.
#[test]
fn a_remembered_split_shows_unified_on_a_phone() {
    let config = Config {
        view: View::Split,
        ..Config::default()
    };
    let phone = |remembered: bool| {
        let mut app = App::new(
            &config,
            Theme::default_theme(),
            "uncommitted".to_owned(),
            Some(diff("a.rs").change),
            (50, 20),
            Indicators::Classic,
        );
        app.set_view_remembered(remembered);
        app
    };
    let mut app = phone(true);
    assert!(!app.doc.layout.split);
    assert_eq!(app.view, View::Split);
    assert!(
        app.take_remembered(true).is_empty(),
        "the saved choice stands"
    );
    app.resize(160, 20);
    assert!(app.doc.layout.split, "split again on a wide screen");
    assert!(phone(false).doc.layout.split);
    // A choice made here is this run's: `s` round to split splits.
    let mut app = phone(true);
    for _ in 0..3 {
        key(&mut app, 's');
    }
    assert_eq!(app.view, View::Split);
    assert!(app.doc.layout.split);
}

/// The chrome takes two rows whatever the screen: the frame's borders,
/// or a title row and a status row; every other row is code.
#[test]
fn the_chrome_takes_two_rows() {
    for (width, height) in [(200, 60), (50, 15)] {
        let mut app = app(width, height);
        key(&mut app, 'X');
        let shown = screen(&mut app);
        assert_eq!(app.doc.height, height - 2, "{width}×{height}");
        let code = (0..app.doc.height)
            .filter(|&y| app.doc.row_at(y).is_some())
            .count();
        assert_eq!(code, height - 2, "{width}×{height}:\n{shown}");
    }
}

#[test]
fn every_key_works_at_forty_by_twelve() {
    let mut app = app(40, 12);
    for c in "nnNNffFFxXXjkgG sbW?q?TT/ab\u{7f}vjjskyYv<>HL0WvG+-".chars() {
        key(&mut app, c);
        app.draw();
    }
    for input in [
        Input::Click { x: 10, y: 5 },
        Input::Drag { x: 10, y: 8 },
        Input::Drag { x: 10, y: 0 },
        Input::Drag { x: 10, y: 11 },
        Input::Release { x: 10, y: 11 },
    ] {
        app.event(Event::Input(input));
        app.draw();
    }
    for key in [
        Key::Tab,
        Key::Esc,
        Key::Down,
        Key::Enter,
        Key::Tab,
        Key::PageDown,
    ] {
        app.event(Event::Input(Input::Key(key, Mods::NONE)));
        app.draw();
    }
    app.resize(20, 5);
    app.draw();
    app.resize(1, 1);
    app.draw();
}

/// The screen as text, the command panel's dimmed text as `<dim>…</>`.
fn panel_markup(app: &mut App) -> String {
    app.draw();
    let dim = app.theme.style(Role::Dim);
    let grid = app.screen.shown_grid();
    (0..grid.height())
        .map(|y| {
            let mut line = String::new();
            let mut in_dim = false;
            for cell in grid.row(y) {
                let dimmed = cell.style() == dim && cell.text() != " ";
                if dimmed != in_dim {
                    line.push_str(if dimmed { "<dim>" } else { "</>" });
                    in_dim = dimmed;
                }
                line.push_str(cell.text());
            }
            if in_dim {
                line.push_str("</>");
            }
            line.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn esc(app: &mut App) {
    app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
}

/// `space` opens the command panel: groups, a row per entry with its
/// state, its key and, dimmed, the direct key when that's another.
#[test]
fn the_command_panel_lists_entries_with_states_and_keys() {
    let mut phone = app(50, 30);
    let mut app = app(140, 40);
    key(&mut app, ' ');
    assert!(app.commands.is_some());
    insta::assert_snapshot!("commands_wide", panel_markup(&mut app));
    key(&mut phone, ' ');
    insta::assert_snapshot!("commands_narrow", screen(&mut phone));
}

/// A row's key runs it at once; a toggle keeps the panel open with its
/// box flipped, somewhere to go (the picker) closes it. `esc` closes.
#[test]
fn a_rows_key_runs_it_and_toggles_keep_the_panel_open() {
    let mut app = app(140, 40);
    key(&mut app, ' ');
    key(&mut app, 'W');
    assert!(app.wrap);
    assert!(app.commands.is_some(), "a toggle keeps it open");
    let shown = screen(&mut app);
    assert!(shown.contains("[x] Wrap long lines"), "{shown}");
    assert!(shown.lines().last().is_some_and(|l| l.contains("wrap on")));
    key(&mut app, 'o');
    assert!(app.commands.is_none(), "the picker closes it");
    assert!(app.picker.is_some());
    esc(&mut app);
    key(&mut app, ' ');
    key(&mut app, 'z');
    assert!(app.commands.is_some(), "a key with no entry keeps it open");
    assert!(status(&mut app).contains("no command"));
    esc(&mut app);
    assert!(app.commands.is_none());
}

/// Browsing: `j`/`k` move a cursor that goes round at the ends; `enter`
/// runs the row under it; `left`/`h` step a setting back, `right`/`l`
/// forward, and so does a tap on its `‹` or `›`.
#[test]
fn browsing_moves_a_cursor_and_enter_runs_its_row() {
    let mut app = app(140, 40);
    key(&mut app, ' ');
    let entries = app.command_entries().len();
    let cursor = |app: &App| app.commands.as_ref().map(Commands::cursor);
    let under = |app: &App| {
        let commands = app.commands.as_ref()?;
        commands.entry(&app.command_entries(), commands.cursor())
    };
    // Up from the first row is the last.
    key(&mut app, 'k');
    assert_eq!(cursor(&app), Some(entries - 1));
    key(&mut app, 'j');
    assert_eq!(cursor(&app), Some(0));
    // To the wrap row, then enter.
    for _ in 0..entries {
        if under(&app) == Some(Act::ToggleWrap) {
            break;
        }
        key(&mut app, 'j');
    }
    assert_eq!(under(&app), Some(Act::ToggleWrap));
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    assert!(app.wrap);
    assert!(app.commands.is_some());
    assert!(screen(&mut app).contains("[x] Wrap long lines"));
    // The layout: auto to split by its key, which puts the cursor on
    // it, then back to auto.
    key(&mut app, 's');
    assert_eq!(app.view, View::Split);
    assert_eq!(under(&app), Some(Act::CycleLayout));
    app.event(Event::Input(Input::Key(Key::Left, Mods::NONE)));
    assert_eq!(app.view, View::Auto);
    key(&mut app, 'h');
    assert_eq!(app.view, View::Unified);
    key(&mut app, 'l');
    assert_eq!(app.view, View::Auto);
    let shown = screen(&mut app);
    let (y, row) = shown
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("Layout ‹"))
        .expect("the layout row");
    let left = row.chars().position(|c| c == '‹').expect("‹");
    let right = row.chars().position(|c| c == '›').expect("›");
    let tap = |app: &mut App, x: usize| {
        app.event(Event::Input(Input::Click {
            x: u16::try_from(x).expect("column"),
            y: u16::try_from(y).expect("row"),
        }));
    };
    tap(&mut app, left);
    assert_eq!(app.view, View::Unified);
    tap(&mut app, right);
    assert_eq!(app.view, View::Auto);
}

/// In the panel `h`/`l` only step values: off a value row they do
/// nothing. Highlighting and line numbers are on `*` and `#`, so they
/// work from any row, the layout row `s` leaves the cursor on included.
#[test]
fn h_and_l_only_step_values_in_the_panel() {
    let mut app = app(140, 40);
    key(&mut app, ' ');
    let (highlight, numbers) = (app.highlight, app.line_numbers);
    key(&mut app, 'h');
    key(&mut app, 'l');
    assert_eq!(
        (app.highlight, app.line_numbers, app.view),
        (highlight, numbers, View::Auto)
    );
    assert!(!status(&mut app).contains("no command"));
    key(&mut app, 's');
    key(&mut app, '*');
    key(&mut app, '#');
    assert_eq!(app.view, View::Split);
    assert_eq!((app.highlight, app.line_numbers), (!highlight, !numbers));
    assert!(app.commands.is_some());
}

/// The theme row opens a list of every theme, grouped; the cursor
/// tries each theme on the screen behind, `esc` brings back the one it
/// opened on, `enter` keeps the one under the cursor and remembers it.
#[test]
fn the_theme_list_tries_themes_live_and_esc_restores() {
    let mut app = app(140, 40);
    // The built-ins only, whatever the machine's theme directory has.
    app.themes = theme::builtin_names()
        .map(|name| {
            let source = theme::dump(name).expect("built-in");
            (
                name.to_owned(),
                Theme::parse(name, &source, &mut Vec::new()),
            )
        })
        .collect();
    key(&mut app, ' ');
    let names = app
        .themes
        .iter()
        .map(|(n, t)| (n.clone(), t.group()))
        .collect();
    app.commands
        .as_mut()
        .expect("open")
        .open_themes(names, "default");
    // The list alone: what's around it is the panel's snapshot.
    let shown = screen(&mut app);
    let lines: Vec<Vec<char>> = shown.lines().map(|l| l.chars().collect()).collect();
    let top = lines
        .iter()
        .position(|l| l.iter().collect::<String>().contains("─ themes ─"))
        .expect("the list");
    let left = lines[top].iter().position(|&c| c == '┌').expect("corner");
    let right = left
        + lines[top][left..]
            .iter()
            .position(|&c| c == '┐')
            .expect("corner");
    let list: Vec<String> = lines[top..]
        .iter()
        .take_while(|l| l.get(left) != Some(&'└'))
        .map(|l| l[left..=right].iter().collect())
        .collect();
    insta::assert_snapshot!("theme_list", list.join("\n"));
    let down = |app: &mut App| app.event(Event::Input(Input::Key(Key::Down, Mods::NONE)));
    down(&mut app);
    down(&mut app);
    assert_eq!(app.theme_name, "github-dark");
    assert_eq!(
        app.theme.style(Role::Added).fg,
        Some(theme::Color::Rgb(0x3f, 0xb9, 0x50))
    );
    esc(&mut app);
    assert_eq!(app.theme_name, "default");
    assert_eq!(app.theme.style(Role::Added).fg, Some(theme::Color::Ansi(2)));
    assert!(
        app.commands.as_ref().is_some_and(|c| !c.themes_open()),
        "back to the panel"
    );
    let names = app
        .themes
        .iter()
        .map(|(n, t)| (n.clone(), t.group()))
        .collect();
    app.commands
        .as_mut()
        .expect("open")
        .open_themes(names, "default");
    for c in "latte".chars() {
        key(&mut app, c);
    }
    assert_eq!(app.theme_name, "catppuccin-latte");
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    assert_eq!(app.theme_name, "catppuccin-latte");
    assert!(
        app.remembered
            .iter()
            .any(|(k, v)| *k == "theme" && v == "catppuccin-latte")
    );
    assert!(app.commands.as_ref().is_some_and(|c| !c.themes_open()));
    // The row's key steps forward and puts the cursor on it; there,
    // left steps back, without the list.
    key(&mut app, 'a');
    assert_eq!(app.theme_name, "gruvbox-light");
    app.event(Event::Input(Input::Key(Key::Left, Mods::NONE)));
    assert_eq!(app.theme_name, "catppuccin-latte");
    assert!(app.commands.as_ref().is_some_and(|c| !c.themes_open()));
}

/// `/` in the panel narrows it with the fuzzy matcher, marking what
/// matched; `esc` clears the filter, then closes.
#[test]
fn the_panel_filters_fuzzily() {
    let mut app = app(140, 40);
    key(&mut app, ' ');
    for c in "/wrp".chars() {
        key(&mut app, c);
    }
    insta::assert_snapshot!("commands_filtered", screen(&mut app));
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    esc(&mut app);
    assert!(app.commands.is_some(), "esc clears the filter first");
    esc(&mut app);
    assert!(app.commands.is_none());
}

#[test]
fn a_tap_on_a_row_runs_it() {
    let mut app = app(140, 40);
    key(&mut app, ' ');
    let shown = screen(&mut app);
    let y = shown
        .lines()
        .position(|l| l.contains("Wrap long lines"))
        .expect("the wrap row");
    app.event(Event::Input(Input::Click {
        x: 70,
        y: u16::try_from(y).expect("row"),
    }));
    assert!(app.wrap);
}

/// The status hint follows focus: the diff's search and a way to the
/// tree, or the tree's filter and a way back, then the menu. On a
/// narrow screen `space` is `␣`, and the pane switch goes first when
/// room runs short.
#[test]
fn the_status_bar_hint_follows_focus() {
    let tab = |app: &mut App| app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
    // The diff-focused frame is the `frame_wide` snapshot.
    let mut wide = app(120, 18);
    let shown = status(&mut wide);
    assert!(
        shown.ends_with("/ search · tab tree · space commands ─┘"),
        "{shown}"
    );
    tab(&mut wide);
    let shown = status(&mut wide);
    assert!(
        shown.ends_with("/ filter · tab diff · space commands ─┘"),
        "{shown}"
    );
    insta::assert_snapshot!("frame_tree_focused", screen(&mut wide));
    let mut phone = app(90, 20);
    let shown = status(&mut phone);
    assert!(
        shown.ends_with("/ search · tab tree · ␣ commands"),
        "{shown}"
    );
    tab(&mut phone);
    let shown = status(&mut phone);
    assert!(
        shown.ends_with("/ filter · tab diff · ␣ commands"),
        "{shown}"
    );
    let mut small = app(60, 20);
    let shown = status(&mut small);
    assert!(
        shown.ends_with("unified    / search · ␣ commands"),
        "{shown}"
    );
    let mut smaller = app(44, 20);
    let shown = status(&mut smaller);
    assert!(shown.ends_with("hunk        ␣ commands"), "{shown}");
}

/// The status bar names the keys in effect: a remapped leader and
/// pane switch show, in both panes.
#[test]
fn the_status_bar_hint_follows_remapped_keys() {
    let mut config = Config::default();
    config.apply_source(
            "keybind = space=unbind\nkeybind = ctrl+k=leader\nkeybind = leader:z=toggle_wrap\nkeybind = tab=unbind\n",
            &mut Vec::new(),
        );
    let mut app = App::new(
        &config,
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(diff("a.rs").change),
        (100, 10),
        Indicators::Classic,
    );
    let shown = status(&mut app);
    assert!(
        shown.ends_with("/ search · shift+tab tree · ctrl+k commands"),
        "{shown}"
    );
    app.event(Event::Input(Input::Key(Key::BackTab, Mods::NONE)));
    let shown = status(&mut app);
    assert!(
        shown.ends_with("/ filter · shift+tab diff · ctrl+k commands"),
        "{shown}"
    );
    app.event(Event::Input(Input::Key(Key::BackTab, Mods::NONE)));
    app.event(Event::Input(Input::Key(Key::Char('k'), Mods::CTRL)));
    assert!(app.commands.is_some());
    key(&mut app, 'z');
    assert!(app.wrap, "the remapped entry ran");
}

/// A modified file whose new side is `text`.
fn searchable(path: &str, text: &str) -> FileDiff {
    let old = Text::new(b"one\ntwo\n".to_vec());
    let new = Text::new(text.as_bytes().to_vec());
    let hunks = hunks(&old, &new, &Options::default());
    let side = Some(Side::Worktree {
        mode: Mode::File,
        id: None,
    });
    FileDiff {
        change: FileChange {
            path: path.into(),
            status: Status::Modified,
            old: side,
            new: side,
            staging: None,
        },
        old,
        new,
        binary: false,
        too_large: false,
        note: None,
        hunks,
    }
}

/// Two files to search: `src/a.rs` with `needle` twice, `src/b.rs`
/// with `Needle` once, both diffed.
fn search_app() -> App {
    let a = searchable("src/a.rs", "one\nneedle here\nand a needle\n");
    let b = searchable("src/b.rs", "Needle\ntwo\n");
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(a.change.clone()),
        (100, 20),
        Indicators::Classic,
    );
    app.event(Event::Enumerated(b.change.clone()));
    app.event(Event::EnumerationDone(None));
    for (i, fd) in [a, b].into_iter().enumerate() {
        let version = app.version(i);
        app.event(Event::Diffed(Done::Diff {
            placed: None,
            file: i,
            version,
            result: Ok(Box::new(loaded(fd))),
        }));
    }
    app
}

/// The search's jobs, done as the workers would: the matches in the
/// file shown, and the counts of the others.
fn work(app: &mut App) {
    for job in app.jobs() {
        let done = match job {
            Job::Find {
                file,
                version,
                diff,
                hidden,
                blocks,
                blocks_version,
                needle,
                id,
                ..
            } => Done::Found {
                file,
                version,
                id,
                matches: crate::tui::worker::find(
                    diff.as_deref(),
                    &hidden,
                    &blocks,
                    &needle,
                    || false,
                ),
                diff,
                hidden,
                blocks_version,
            },
            Job::Search {
                file,
                version,
                needle,
                id,
                ..
            } => Done::Search {
                file,
                version,
                id,
                count: Some(needle.find_all(b"Needle\n").len()),
            },
            _ => continue,
        };
        app.event(Event::Diffed(done));
    }
}

/// The status bar: the bottom row.
fn status(app: &mut App) -> String {
    screen(app).lines().last().unwrap_or_default().to_owned()
}

/// `/` searches as it's typed, the workers marking matches on screen
/// as they find them; `enter` goes to the first; `n`/`N` go between
/// matches and on into the next file; the status bar counts; `esc`
/// gives `n` back to changes.
#[test]
fn the_diff_search_marks_jumps_and_counts() {
    let mut app = search_app();
    app.draw();
    for c in "/needle".chars() {
        key(&mut app, c);
    }
    assert!(
        app.doc.document.search.is_none(),
        "typing waits for no scan"
    );
    assert!(screen(&mut app).contains("… in this file"));
    work(&mut app);
    let marks = app
        .doc
        .document
        .search
        .as_ref()
        .map(|m| m.matches.list.len());
    assert_eq!(marks, Some(2), "marked once found");
    app.draw();
    let lit = app.theme.style(Role::SearchMatch);
    let cells = (0..20)
        .flat_map(|y| app.screen.shown_grid().row(y).iter())
        .filter(|c| c.style() == lit)
        .count();
    assert_eq!(
        cells,
        2 * "needle".len(),
        "both matches drawn in the match style"
    );
    let shown = status(&mut app);
    assert!(shown.contains("/needle▏  2 in this file"), "{shown}");
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    work(&mut app);
    let shown = status(&mut app);
    assert!(shown.contains("match 1/3"), "{shown}");
    key(&mut app, 'n');
    let shown = status(&mut app);
    assert!(shown.contains("match 2/3"), "{shown}");
    // On into src/b.rs, where smart case still finds `Needle`, once
    // its matches arrive.
    key(&mut app, 'n');
    assert_eq!(app.doc.document.pos.file, 1);
    work(&mut app);
    let shown = status(&mut app);
    assert!(shown.contains("match 3/3"), "{shown}");
    key(&mut app, 'N');
    assert_eq!(app.doc.document.pos.file, 0);
    esc(&mut app);
    assert!(app.search.is_none());
    key(&mut app, 'g');
    key(&mut app, 'n');
    assert!(app.search.is_none(), "n goes to changes again");
}

/// `/` finds comments' text with the code's: `enter` goes to the first
/// match, on the line, and `n` to the next, in the box under it; one
/// only in a comment is found too.
#[test]
fn the_diff_search_finds_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    let current = |app: &App| app.doc.document.search.as_ref().and_then(|m| m.current);
    typed(&mut app, "/fifty");
    work(&mut app);
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    assert!(
        status(&mut app).contains("match 1/2"),
        "{}",
        status(&mut app)
    );
    let top = app.doc.document.pos;
    key(&mut app, 'n');
    assert!(
        status(&mut app).contains("match 2/2"),
        "{}",
        status(&mut app)
    );
    assert_eq!(current(&app), Some(1));
    assert!(app.doc.document.pos >= top, "the box is under its line");
    let shown = screen(&mut app);
    let card = shown
        .lines()
        .find(|l| l.contains("Fifty?"))
        .expect("on screen");
    assert!(card.contains('▸'), "{shown}");
    esc(&mut app);
    key(&mut app, 'g');
    typed(&mut app, "/why two");
    work(&mut app);
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    assert_eq!(app.search_status().as_deref(), Some("match 1/1"));
    assert!(screen(&mut app).contains("Why two?"));
}

/// A scan for a cleared search that reports after the next search
/// started is dropped: search ids never repeat.
#[test]
fn a_late_count_from_a_cleared_search_isnt_taken_for_the_next_one() {
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(searchable("a.rs", "f\n").change),
        (100, 20),
        Indicators::Classic,
    );
    app.event(Event::EnumerationDone(None));
    typed(&mut app, "/f");
    let old = app.search.as_ref().map(|s| s.id).expect("searching");
    app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
    typed(&mut app, "/g");
    app.event(Event::Diffed(Done::Search {
        file: 0,
        version: app.version(0),
        id: old,
        count: Some(5),
    }));
    let counts = app.search.as_ref().map(|s| s.counts.clone());
    assert_eq!(counts, Some(vec![None]));
}

/// A search typed while the list is still arriving survives the sort
/// at the end of it: an untracked file listed last moves ahead of the
/// file shown, and the matches stay on the file they were found in.
#[test]
fn search_marks_follow_their_file_when_the_list_is_sorted() {
    let tracked = searchable("src/z.rs", "needle\nneedle\n");
    let untracked = FileDiff {
        change: FileChange {
            status: Status::Untracked,
            old: None,
            ..searchable("src/a.rs", "").change
        },
        ..searchable("src/a.rs", "a needle\n")
    };
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(tracked.change.clone()),
        (100, 20),
        Indicators::Classic,
    );
    app.event(Event::Enumerated(untracked.change.clone()));
    for (file, fd) in [tracked, untracked].into_iter().enumerate() {
        let version = app.version(file);
        app.event(Event::Diffed(Done::Diff {
            placed: None,
            file,
            version,
            result: Ok(Box::new(loaded(fd))),
        }));
    }
    app.draw();
    typed(&mut app, "/needle");
    let stale_jobs = app.jobs();
    work(&mut app);
    assert_eq!(app.doc.document.search.as_ref().map(|m| m.file), Some(0));

    app.event(Event::EnumerationDone(None));
    assert_eq!(app.doc.document.files()[0].change().path, "src/a.rs");
    assert_eq!(app.doc.document.pos.file, 1, "still on src/z.rs");
    let marked = app
        .doc
        .document
        .search
        .as_ref()
        .map(|m| &app.doc.document.files()[m.file].change().path);
    assert!(marked.is_none_or(|path| path == "src/z.rs"), "{marked:?}");
    // A scan started before the sort reports by the old positions.
    for job in stale_jobs {
        if let Job::Find {
            file,
            version,
            diff,
            hidden,
            blocks,
            blocks_version,
            needle,
            id,
            ..
        } = job
        {
            app.event(Event::Diffed(Done::Found {
                file,
                version,
                id,
                matches: crate::tui::worker::find(
                    diff.as_deref(),
                    &hidden,
                    &blocks,
                    &needle,
                    || false,
                ),
                diff,
                hidden,
                blocks_version,
            }));
        }
    }
    work(&mut app);
    let marks = app
        .doc
        .document
        .search
        .as_ref()
        .map(|m| (m.file, m.matches.list.len()));
    assert_eq!(marks, Some((1, 2)), "src/z.rs's two matches");
    app.draw();
    let lit = app.theme.style(Role::SearchMatch);
    let cells = (0..20)
        .flat_map(|y| app.screen.shown_grid().row(y).iter())
        .filter(|c| c.style() == lit)
        .count();
    assert_eq!(cells, 2 * "needle".len());
    let counts = app.search.as_ref().map(|s| s.counts.clone());
    assert_eq!(counts.and_then(|c| c.first().copied()), Some(Some(1)));
}

/// A file whose diff failed has nothing to scan: `enter` and `n` on it
/// go on to the files that have matches instead of waiting.
#[test]
fn a_search_on_a_failed_file_goes_on_to_the_others() {
    let mut app = search_app();
    app.event(Event::Enumerated(diff("src/c.rs").change));
    app.event(Event::Diffed(Done::Diff {
        placed: None,
        file: 2,
        version: app.version(2),
        result: Err("unreadable".to_owned()),
    }));
    key(&mut app, 'f');
    key(&mut app, 'f');
    assert_eq!(app.doc.document.pos.file, 2);
    typed(&mut app, "/needle");
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    work(&mut app);
    assert!(!status(&mut app).contains("searching this file"));
    key(&mut app, 'N');
    work(&mut app);
    assert_eq!(
        app.doc.document.pos.file, 1,
        "back to src/b.rs and its match"
    );
    let current = app.doc.document.search.as_ref().and_then(|m| m.current);
    assert_eq!(current, Some(0), "{}", status(&mut app));
}

/// Side by side, a removed line and the added line beside it share a
/// row: `n` takes matches in the order the rows show them, left to
/// right, not in the order of the diff's lines, and `n` into a file
/// lands on the first row's.
#[test]
fn n_goes_through_matches_in_row_order_side_by_side() {
    let split = |old: &[u8], new: &[u8]| {
        let (old, new) = (Text::new(old.to_vec()), Text::new(new.to_vec()));
        let side = Some(Side::Worktree {
            mode: Mode::File,
            id: None,
        });
        let fd = FileDiff {
            change: FileChange {
                path: "src/a.rs".into(),
                status: Status::Modified,
                old: side,
                new: side,
                staging: None,
            },
            hunks: hunks(&old, &new, &Options::default()),
            old,
            new,
            binary: false,
            too_large: false,
            note: None,
        };
        let mut app = app_of(&Config::default(), "working tree", fd, (160, 20));
        assert!(app.doc.layout.split);
        typed(&mut app, "/needle");
        app
    };
    let current = |app: &App| app.doc.document.search.as_ref().and_then(|m| m.current);
    let mut app = split(b"a needle 1\nb needle 2\n", b"a needle 1!\nb needle 2!\n");
    work(&mut app);
    press(&mut app, Key::Enter);
    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.extend(current(&app));
        key(&mut app, 'n');
    }
    // The diff has both removed lines, then both added ones.
    assert_eq!(seen, [0, 2, 1, 3]);

    // The removed match is the diff's first, on the second row.
    let mut app = split(b"x\nb needle\n", b"a needle\ny\n");
    if let Some(search) = &mut app.search {
        search.pending = Some((0, Land::First));
    }
    work(&mut app);
    assert_eq!(current(&app), Some(1));
}

/// A pattern typed over a running scan stops it: the scan asks, and
/// the search's id has moved on.
#[test]
fn a_newer_pattern_stops_the_scan_of_an_older_one() {
    use crate::tui::worker::find;
    let mut app = search_app();
    typed(&mut app, "/ne");
    let jobs = app.jobs();
    let Some(Job::Find {
        diff,
        hidden,
        live,
        id,
        ..
    }) = jobs.into_iter().find(|j| matches!(j, Job::Find { .. }))
    else {
        panic!("a scan of the file shown");
    };
    key(&mut app, 'e');
    let stale = || live.load(Ordering::Relaxed) != id;
    assert!(stale());
    let needle = Needle::new("ne");
    let diff = diff.as_deref();
    assert_eq!(find(diff, &hidden, &[], &needle, stale), None);
    assert!(find(diff, &hidden, &[], &needle, || false).is_some());
}

/// `/` in the tree filters it fuzzily: matching files with their
/// folders, the matched letters marked; `f` keeps to them; `esc`
/// clears it.
#[test]
fn the_tree_filter_scopes_moves_and_clears() {
    let mut app = files_app(120, 12);
    app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
    for c in "/fair\r".chars() {
        app.event(Event::Input(Input::Key(
            if c == '\r' { Key::Enter } else { Key::Char(c) },
            Mods::NONE,
        )));
    }
    let shown = screen(&mut app);
    assert!(shown.contains("Files · 1/4 · fair"), "{shown}");
    assert!(shown.contains("▼ kernel/sched/"), "{shown}");
    assert!(!shown.contains(".clang-format"), "{shown}");
    assert!(
        shown.lines().next().is_some_and(|l| l.contains("fair.c")),
        "{shown}"
    );
    app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
    let at = app.doc.document.pos.file;
    key(&mut app, 'f');
    assert_eq!(
        app.doc.document.pos.file, at,
        "no other file passes the filter"
    );
    app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
    esc(&mut app);
    let shown = screen(&mut app);
    assert!(shown.contains(".clang-format"), "{shown}");
    assert!(shown.contains("tree filter cleared"), "{shown}");
}

/// `esc` backs out a layer at a time: the overlay, the search, the
/// selection, the tree's filter, the tree's focus; then it only says
/// how to quit.
#[test]
fn esc_backs_out_one_layer_at_a_time() {
    let mut app = app(100, 20);
    app.draw();
    let esc = |app: &mut App| app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
    typed(&mut app, "/line");
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    key(&mut app, 'v');
    key(&mut app, '?');
    assert_eq!(esc(&mut app), None);
    assert!(app.help.is_none(), "the overlay first");
    assert!(app.search.is_some() && app.doc.selection.is_some());
    esc(&mut app);
    assert!(app.search.is_none(), "then the search");
    assert!(app.doc.selection.is_some());
    esc(&mut app);
    assert!(app.doc.selection.is_none(), "then the selection");
    app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
    typed(&mut app, "/zz");
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    assert_eq!(app.tree.filter(), "zz");
    esc(&mut app);
    assert_eq!(app.tree.filter(), "", "then the tree's filter");
    assert_eq!(app.pane, Pane::Tree);
    esc(&mut app);
    assert_eq!(app.pane, Pane::Diff, "then the tree's focus");
    assert_eq!(esc(&mut app), None, "never quits");
    let status = status(&mut app);
    assert!(status.contains("press q to quit"), "{status}");
}

/// `keybind = esc=quit`: `esc` still backs out of the search, the
/// selection and the tree first, and quits only with nothing left.
#[test]
fn esc_bound_to_quit_backs_out_of_layers_first() {
    let mut config = Config::default();
    config.apply_source("keybind = esc=quit\n", &mut Vec::new());
    let mut app = App::new(
        &config,
        Theme::default_theme(),
        "uncommitted".to_owned(),
        Some(diff("a.rs").change),
        (120, 20),
        Indicators::Classic,
    );
    app.draw();
    let esc = |app: &mut App| app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
    typed(&mut app, "/line");
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    key(&mut app, '?');
    assert_eq!(esc(&mut app), None);
    assert!(app.help.is_none(), "the overlay first");
    assert_eq!(esc(&mut app), None);
    assert!(app.search.is_none(), "then the search");
    app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
    assert_eq!(esc(&mut app), None);
    assert_eq!(app.pane, Pane::Diff, "then the tree's focus");
    assert_eq!(esc(&mut app), Some(Action::Quit), "then it quits");
}

/// The panel's copy copies the selection made before it opened.
#[test]
fn the_panels_copy_copies_the_selection() {
    let mut app = app(140, 40);
    app.draw();
    for c in "vj ".chars() {
        key(&mut app, c);
    }
    assert!(app.commands.is_some());
    let copied = app.event(Event::Input(Input::Key(Key::Char('y'), Mods::NONE)));
    assert_eq!(
        copied,
        Some(Action::Copy {
            text: b"line 1\nline 2".to_vec(),
            note: "copied 2 lines".to_owned(),
        })
    );
}

/// Highlighting or whitespace toggled from the panel diffs every file
/// again with the new settings.
#[test]
fn toggling_highlighting_diffs_again() {
    let mut app = app(140, 40);
    app.draw();
    let version = app.version(0);
    key(&mut app, ' ');
    let done = app.event(Event::Input(Input::Key(Key::Char('*'), Mods::NONE)));
    assert_eq!(
        done,
        Some(Action::Rediff {
            highlight: false,
            whitespace: Whitespace::Exact
        })
    );
    assert_ne!(app.version(0), version, "work asked for before is void");
    assert!(
        app.doc.document.files()[0].wants_diff(),
        "the shown file is diffed again"
    );
    let done = app.event(Event::Input(Input::Key(Key::Char('I'), Mods::NONE)));
    assert_eq!(
        done,
        Some(Action::Rediff {
            highlight: false,
            whitespace: Whitespace::IgnoreAll
        })
    );
}

/// `X` switches every file, not just the one shown, and is remembered
/// as `context`: `full`, or the number of lines it goes back to. The
/// whitespace toggle sets both whitespace keys; `w` remembers watching.
#[test]
fn whole_files_whitespace_and_watch_are_remembered() {
    let mut app = app(120, 18);
    app.set_context_lines(5);
    key(&mut app, 'X');
    assert!(
        app.doc
            .document
            .files()
            .iter()
            .all(|e| app.doc.document.is_full(e))
    );
    assert_eq!(app.take_remembered(true), [("context", "full".to_owned())]);
    key(&mut app, 'X');
    assert_eq!(app.take_remembered(true), [("context", "5".to_owned())]);
    key(&mut app, ' ');
    key(&mut app, 'I');
    esc(&mut app);
    key(&mut app, 'w');
    assert_eq!(
        app.take_remembered(true),
        [
            ("ignore-all-space", "true".to_owned()),
            ("ignore-space-change", "false".to_owned()),
            ("watch", "true".to_owned())
        ]
    );
    // A watch that couldn't start isn't remembered.
    key(&mut app, 'w');
    key(&mut app, 'w');
    app.set_watching(false);
    assert_eq!(app.take_remembered(true), []);
    app.set_piped();
    key(&mut app, 'X');
    assert_eq!(app.take_remembered(true), []);
}

/// `tree = hide` starts without the tree even on a wide screen; `t`
/// shows it, and that choice is remembered.
#[test]
fn tree_hide_starts_hidden_and_t_shows_it() {
    let config = Config {
        tree: TreeMode::Hide,
        ..Config::default()
    };
    let mut app = App::new(
        &config,
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(diff("a.rs").change),
        (120, 10),
        Indicators::Classic,
    );
    assert!(!screen(&mut app).contains("┌ Files"));
    key(&mut app, 't');
    assert!(screen(&mut app).contains("┌ Files"));
    assert_eq!(app.take_remembered(true), [("tree", "show".to_owned())]);
}

/// `tree = show` opens the tree on a narrow screen too, over the diff
/// and with the keys; `auto`, or a `show` remembered from a wide
/// screen, starts without it there.
#[test]
fn tree_show_opens_it_over_a_narrow_screen_unless_remembered() {
    let start = |tree, remembered| {
        let config = Config {
            tree,
            ..Config::default()
        };
        let mut app = App::new(
            &config,
            Theme::default_theme(),
            "working tree".to_owned(),
            Some(diff("a.rs").change),
            (50, 10),
            Indicators::Classic,
        );
        app.start_tree(remembered);
        (app.tree_shown, app.pane)
    };
    assert_eq!(start(TreeMode::Show, false), (true, Pane::Tree));
    assert_eq!(start(TreeMode::Show, true), (false, Pane::Diff));
    assert_eq!(start(TreeMode::Auto, false), (false, Pane::Diff));
}

/// Without files, a tree over the diff still says why the list is
/// empty.
#[test]
fn an_empty_tree_over_the_diff_shows_the_empty_note() {
    let config = Config {
        tree: TreeMode::Show,
        ..Config::default()
    };
    let mut app = App::new(
        &config,
        Theme::default_theme(),
        "working tree".to_owned(),
        None,
        (50, 12),
        Indicators::Classic,
    );
    app.start_tree(false);
    assert!(screen(&mut app).contains("listing changes…"));
    app.event(Event::EnumerationDone(None));
    let text = screen(&mut app);
    assert!(
        text.contains("┌ Files") && text.contains("no changes"),
        "{text}"
    );
}

/// A remembered split on a screen too narrow for it renders unified,
/// and nothing is written over the choice; only `s` changes it.
#[test]
fn a_remembered_split_renders_unified_when_narrow_and_stays_remembered() {
    let config = Config {
        view: View::Split,
        ..Config::default()
    };
    let first = diff("a.rs");
    let mut app = App::new(
        &config,
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(first.change.clone()),
        (30, 10),
        Indicators::Classic,
    );
    app.event(Event::EnumerationDone(None));
    app.event(Event::Diffed(Done::Diff {
        placed: None,
        file: 0,
        version: app.version(0),
        result: Ok(Box::new(loaded(first))),
    }));
    // Split, a row shows its line on both halves; unified, once.
    let halves = |app: &mut App| {
        let shown = screen(app);
        let row = shown
            .lines()
            .find(|l| l.contains("line 1"))
            .unwrap_or_default()
            .to_owned();
        row.matches("line 1").count()
    };
    assert_eq!(halves(&mut app), 1, "too narrow for halves");
    app.resize(160, 10);
    assert_eq!(halves(&mut app), 2);
    app.resize(30, 10);
    assert_eq!(halves(&mut app), 1);
    assert_eq!(app.view, View::Split);
    assert_eq!(app.take_remembered(true), []);
    key(&mut app, 's');
    assert_eq!(app.take_remembered(true), [("view", "unified".to_owned())]);
}

/// Line numbers and hunk numbers off: the change marks and the
/// functions stay.
#[test]
fn line_numbers_and_hunk_headers_can_be_hidden() {
    let mut app = app(60, 12);
    for c in [' ', '#', '@'] {
        key(&mut app, c);
    }
    esc(&mut app);
    let shown = screen(&mut app);
    insta::assert_snapshot!(shown);
    assert_eq!(
        app.take_remembered(true),
        [
            ("line-numbers", "false".to_owned()),
            ("hunk-headers", "false".to_owned())
        ]
    );
}

fn copy(app: &mut App) -> Option<Action> {
    app.event(Event::Input(Input::Key(Key::Char('y'), Mods::NONE)))
}

/// `v` `j` `y` copies the code of the lines selected, nothing else; a
/// drag selects too; `Y` copies the path.
#[test]
fn v_j_y_and_a_drag_copy_lines_and_shift_y_the_path() {
    let mut app = app(100, 20);
    app.draw();
    for c in "vjj".chars() {
        key(&mut app, c);
    }
    let shown = status(&mut app);
    assert!(shown.contains("3 new lines · y copies"), "{shown}");
    assert_eq!(
        copy(&mut app),
        Some(Action::Copy {
            text: b"line 1\nline 2\nline 3".to_vec(),
            note: "copied 3 lines".to_owned(),
        })
    );
    assert_eq!(copy(&mut app), None, "the copy ends the selection");
    // Pressed in the code of `line 4` (row 5, under the frame's top and
    // the hunk's `@@` row) and dragged two rows down: by characters, on
    // the old side, since the press was on a removed line; copied as
    // the button is let go, and again by `y`.
    app.draw();
    app.event(Event::Input(Input::Click { x: 40, y: 5 }));
    app.event(Event::Input(Input::Drag { x: 40, y: 7 }));
    let chars = Some(Action::Copy {
        text: b"ne 4\nlin".to_vec(),
        note: "copied 2 lines".to_owned(),
    });
    assert_eq!(
        app.event(Event::Input(Input::Release { x: 40, y: 7 })),
        chars
    );
    assert_eq!(copy(&mut app), chars);
    // Dragged into the new side's half, it keeps to the old side: to
    // the end of the line there.
    app.draw();
    app.clicks = None;
    app.event(Event::Input(Input::Click { x: 40, y: 5 }));
    app.event(Event::Input(Input::Drag { x: 90, y: 5 }));
    assert_eq!(
        app.event(Event::Input(Input::Release { x: 90, y: 5 })),
        Some(Action::Copy {
            text: b"ne 4".to_vec(),
            note: "copied 4 chars".to_owned(),
        })
    );
    // A double click selects the word, a triple click the line.
    for (clicks, text) in [(2, &b"line"[..]), (3, b"line 4")] {
        app.draw();
        app.clicks = None;
        let mut copied = None;
        for _ in 0..clicks {
            app.event(Event::Input(Input::Click { x: 40, y: 5 }));
            copied = app.event(Event::Input(Input::Release { x: 40, y: 5 }));
        }
        let Some(Action::Copy { text: got, .. }) = copied else {
            panic!("{clicks} clicks copy: {copied:?}");
        };
        assert_eq!(got, text);
        app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
    }
    app.clicks = None;
    // The same drag over the line numbers selects whole lines.
    app.draw();
    app.event(Event::Input(Input::Click { x: 30, y: 5 }));
    app.event(Event::Input(Input::Drag { x: 30, y: 7 }));
    assert_eq!(
        app.event(Event::Input(Input::Release { x: 30, y: 7 })),
        Some(Action::Copy {
            text: b"line 4\nline 5".to_vec(),
            note: "copied 2 lines".to_owned(),
        })
    );
    assert_eq!(
        app.event(Event::Input(Input::Key(Key::Char('Y'), Mods::NONE))),
        Some(Action::Copy {
            text: b"src/some/deeply/nested/module/file.rs".to_vec(),
            note: "copied src/some/deeply/nested/module/file.rs".to_owned(),
        })
    );
}

/// A click, then a shift-click, selects from the one to the other,
/// copied as the button is let go; another shift-click stretches the
/// selection further.
#[test]
fn a_shift_click_extends_the_selection() {
    let mut app = app(100, 20);
    app.draw();
    app.event(Event::Input(Input::Click { x: 30, y: 5 }));
    app.event(Event::Input(Input::Release { x: 30, y: 5 }));
    app.event(Event::Input(Input::ShiftClick { x: 30, y: 7 }));
    let lines = |text: &[u8], n| {
        Some(Action::Copy {
            text: text.to_vec(),
            note: format!("copied {n} lines"),
        })
    };
    assert_eq!(
        app.event(Event::Input(Input::Release { x: 30, y: 7 })),
        lines(b"line 4\nline 5", 2)
    );
    app.draw();
    app.event(Event::Input(Input::ShiftClick { x: 30, y: 8 }));
    assert_eq!(
        app.event(Event::Input(Input::Release { x: 30, y: 8 })),
        lines(b"line 4\nline 5\nline 6", 3)
    );
    // A press on another file isn't one to select from.
    key(&mut app, 'f');
    app.draw();
    app.event(Event::Input(Input::ShiftClick { x: 30, y: 7 }));
    assert_eq!(
        app.event(Event::Input(Input::Release { x: 30, y: 7 })),
        None
    );
    assert!(app.doc.selection.is_none());
}

/// `A`, `U` and `D` hand the hunk at the top, or the selected lines, to
/// the loop, as far as the file's staging leaves anything to do; `D`
/// asks first, and any key but `y` keeps the lines. On a diff that isn't
/// of uncommitted changes they only say so.
#[test]
fn stage_unstage_and_discard_take_the_hunk_or_the_selection() {
    let mut app = app(100, 20);
    app.draw();
    key(&mut app, 'A');
    assert!(status(&mut app).contains("uncommitted changes"));
    app.set_uncommitted(Some(Uncommitted::Unstaged));
    let Some(Action::EditLines {
        diff: shown,
        picked,
        edit: Edit::Stage,
    }) = press(&mut app, Key::Char('A'))
    else {
        panic!("A stages");
    };
    assert_eq!(picked, Picked::hunk(&shown.hunks[0]));
    app.event(Event::Edited(Ok("staged 2 lines".to_owned())));
    key(&mut app, 'U');
    assert!(status(&mut app).contains("nothing here is staged"));

    // `D` first has the loop work out what goes, which the question
    // names: the lines that differ from the index, and where.
    let plan = |app: &mut App| {
        let Some(Action::PlanDiscard { diff, picked }) = press(app, Key::Char('D')) else {
            panic!("D plans first");
        };
        app.event(Event::Planned {
            diff,
            picked,
            result: Ok(Applied {
                lines: 1,
                deleted: false,
                at: Some((4, 4)),
            }),
        });
    };
    plan(&mut app);
    assert_eq!(
        note(&app).as_deref(),
        Some("discard 1 line at line 4 of src/some/deeply/nested/module/file.rs? y/n")
    );
    key(&mut app, 'j');
    assert!(status(&mut app).contains("kept"));
    plan(&mut app);
    let discard = press(&mut app, Key::Char('y'));
    assert!(
        matches!(
            discard,
            Some(Action::EditLines {
                edit: Edit::Discard,
                ..
            })
        ),
        "{discard:?}"
    );
    app.event(Event::Edited(Ok("discarded 1 line".to_owned())));

    // `v` and three `j` reach the change's row: both its lines go, and
    // the selection ends.
    for c in "vjjj".chars() {
        key(&mut app, c);
    }
    let Some(Action::EditLines { picked, .. }) = press(&mut app, Key::Char('A')) else {
        panic!("A stages the selection");
    };
    assert_eq!(
        (picked.old.contains(&3), picked.new.contains(&3)),
        (true, true)
    );
    assert!(app.doc.selection.is_none());
    app.event(Event::Edited(Ok("staged 2 lines".to_owned())));
    assert!(status(&mut app).contains("staged 2 lines"));

    // In plain dffy a staged file only unstages.
    let staged = FileDiff {
        change: FileChange {
            staging: Some(Staging::Staged),
            ..diff("src/a.rs").change
        },
        ..diff("src/a.rs")
    };
    let mut app = app_of(&Config::default(), "working tree", staged, (100, 20));
    app.set_uncommitted(Some(Uncommitted::Plain));
    app.draw();
    key(&mut app, 'A');
    assert!(status(&mut app).contains("staged already"));
    assert!(matches!(
        press(&mut app, Key::Char('U')),
        Some(Action::EditLines {
            edit: Edit::Unstage,
            ..
        })
    ));
}

/// `D`'s question comes from the loop a moment after the key. Input in
/// between, which may open something a `y` would then answer, drops it
/// with a word; a release, the end of the click that asked, doesn't.
#[test]
fn a_discard_plan_after_other_input_asks_nothing() {
    let mut app = app(100, 20);
    app.set_uncommitted(Some(Uncommitted::Unstaged));
    app.draw();
    let planned = |diff, picked| Event::Planned {
        diff,
        picked,
        result: Ok(Applied {
            lines: 1,
            deleted: false,
            at: Some((4, 4)),
        }),
    };
    let Some(Action::PlanDiscard { diff, picked }) = press(&mut app, Key::Char('D')) else {
        panic!("D plans first");
    };
    key(&mut app, 'j');
    app.event(planned(diff, picked));
    assert!(app.confirm.is_none());
    assert!(
        status(&mut app).contains("discard dropped"),
        "{}",
        status(&mut app)
    );
    assert_eq!(press(&mut app, Key::Char('y')), None);

    let Some(Action::PlanDiscard { diff, picked }) = press(&mut app, Key::Char('D')) else {
        panic!("D plans again");
    };
    app.event(Event::Input(Input::Release { x: 30, y: 7 }));
    app.event(planned(diff, picked));
    assert!(app.confirm.is_some(), "asked");
}

/// One git operation at a time: a second `A` before the first says how
/// it went is turned away, the selection kept for when it has; and so
/// is a `D` or a write.
#[test]
fn a_second_edit_waits_for_the_first() {
    let mut app = app(100, 20);
    app.set_uncommitted(Some(Uncommitted::Unstaged));
    app.draw();
    assert!(matches!(
        press(&mut app, Key::Char('A')),
        Some(Action::EditLines { .. })
    ));
    for c in "vjjj".chars() {
        key(&mut app, c);
    }
    assert_eq!(press(&mut app, Key::Char('A')), None);
    assert!(status(&mut app).contains("still applying; wait"));
    assert!(app.doc.selection.is_some(), "kept for the next press");
    assert_eq!(press(&mut app, Key::Char('D')), None);
    assert!(status(&mut app).contains("still applying; wait"));
    app.event(Event::Edited(Err("it failed".to_owned())));
    assert!(matches!(
        press(&mut app, Key::Char('A')),
        Some(Action::EditLines { .. })
    ));
}

/// In the split view, a selection stages, unstages or discards its own
/// side's lines: the half it's on, not the line beside it.
#[test]
fn a_split_selection_takes_its_own_side() {
    let config = Config {
        view: View::Split,
        ..Config::default()
    };
    let mut app = app_of(&config, "working tree", diff("a.rs"), (160, 20));
    app.set_uncommitted(Some(Uncommitted::Unstaged));
    app.draw();
    let mut select = |sides: &str| {
        for c in sides.chars() {
            key(&mut app, c);
        }
        let Some(Action::EditLines { picked, .. }) = press(&mut app, Key::Char('A')) else {
            panic!("A stages the selection");
        };
        app.event(Event::Edited(Ok("staged".to_owned())));
        (picked.old.contains(&3), picked.new.contains(&3))
    };
    let first = select("vjjj");
    let other = select("vjjjs");
    assert!(
        first == (true, false) || first == (false, true),
        "{first:?}"
    );
    assert_eq!(other, (first.1, first.0), "s picks the other half");
}

/// `n` into a file not diffed yet waits at its top; `n` again waits
/// too, rather than skipping the file's changes; the diff arriving
/// opens the file as it opens the first time, its first change in view
/// (on the first screen here, so from the top), and the next `n` goes
/// on past that change.
#[test]
fn n_into_a_file_not_diffed_yet_waits_for_its_diff() {
    let mut app = app(100, 16);
    for _ in 0..3 {
        key(&mut app, 'n');
    }
    assert_eq!(
        (app.doc.document.pos.file, app.doc.document.pos.part),
        (1, 0)
    );
    key(&mut app, 'n');
    assert_eq!(
        (app.doc.document.pos.file, app.doc.document.pos.part),
        (1, 0)
    );
    assert!(screen(&mut app).contains("waiting for this file's diff"));
    let loaded = loaded(diff("src/zz.rs"));
    app.event(Event::Diffed(Done::Diff {
        placed: None,
        file: 1,
        version: app.version(1),
        result: Ok(Box::new(loaded)),
    }));
    assert_eq!(app.doc.document.pos.file, 1);
    assert!(app.doc.document.pos.part > 0, "{:?}", app.doc.document.pos);
    let shown = screen(&mut app);
    let top = shown.lines().nth(1).unwrap_or_default();
    assert!(top.contains("@@ -1,7"), "{shown}");
    assert!(shown.contains("-│ line 4"), "{shown}");
    // The second change is near the end: the view goes as far down as
    // the file goes.
    key(&mut app, 'n');
    let shown = screen(&mut app);
    let top = shown.lines().nth(1).unwrap_or_default();
    assert!(top.contains("+│ line four"), "{shown}");
    assert!(shown.contains("-│ line 50"), "{shown}");
}

/// With `context = full`, `f` opens a file whose first change is at
/// line 400 a few rows above that change, whether its diff came before
/// or after; a tap's release meanwhile changes nothing.
#[test]
fn a_full_file_opens_at_its_first_change() {
    let late = || {
        let old = (1..=500)
            .map(|i| format!("line {i}\n"))
            .collect::<Vec<_>>()
            .concat();
        let new = old.replace("line 400\n", "line four hundred\n");
        FileDiff {
            hunks: hunks(
                &Text::new(old.clone().into()),
                &Text::new(new.clone().into()),
                &Options::default(),
            ),
            old: Text::new(old.into()),
            new: Text::new(new.into()),
            ..diff("src/zz.rs")
        }
    };
    let mut config = Config::default();
    config.apply_source("context = full\n", &mut Vec::new());
    for diff_first in [true, false] {
        let mut app = app_of(&config, "working tree", diff("src/a.rs"), (100, 16));
        let deliver = |app: &mut App| {
            app.event(Event::Diffed(Done::Diff {
                placed: None,
                file: 1,
                version: app.version(1),
                result: Ok(Box::new(loaded(late()))),
            }));
        };
        if diff_first {
            deliver(&mut app);
            key(&mut app, 'f');
        } else {
            key(&mut app, 'f');
            app.event(Event::Input(Input::Release { x: 50, y: 5 }));
            deliver(&mut app);
        }
        let shown = screen(&mut app);
        let first = shown.lines().nth(1).unwrap_or_default();
        assert!(first.contains("397 397"), "{shown}");
        assert!(shown.contains("-│ line 400"), "{shown}");
    }
}

/// `keybind` lines in the config change what keys do in the viewer,
/// and the help lists the keys as they are.
#[test]
fn remapped_keys_act_and_the_help_shows_them() {
    let mut config = Config::default();
    let mut warnings = Vec::new();
    config.apply_source(
            "keybind = n=next_file\nkeybind = q=unbind\nkeybind = ctrl+q=quit\nkeybind = ?=unbind\nkeybind = F1=help\n",
            &mut warnings,
        );
    assert_eq!(warnings.len(), 1, "F1 isn't a key name: {warnings:?}");
    config.apply_source("keybind = f1=help\n", &mut warnings);
    let first = diff("a.rs");
    let mut app = App::new(
        &config,
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(first.change),
        (100, 46),
        Indicators::Classic,
    );
    app.event(Event::Enumerated(diff("b.rs").change));
    app.event(Event::EnumerationDone(None));
    key(&mut app, 'n');
    assert_eq!(app.doc.document.pos.file, 1, "n moves to the next file now");
    assert_eq!(
        app.event(Event::Input(Input::Key(Key::Char('q'), Mods::NONE))),
        None
    );
    app.event(Event::Input(Input::Key(Key::F(1), Mods::NONE)));
    let has = |app: &mut App, row: &str| {
        let shown = screen(app);
        let found = shown.lines().any(|l| {
            l.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains(row)
        });
        assert!(found, "{row}:\n{shown}");
    };
    has(&mut app, "} next change");
    typed(&mut app, "2");
    has(&mut app, "│ f n next file");
    typed(&mut app, "/quit");
    has(&mut app, "│ ctrl+c ctrl+q quit Other");
    app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
    typed(&mut app, "/help");
    has(&mut app, "│ f1 show this help Other");
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    app.event(Event::Input(Input::Key(Key::F(1), Mods::NONE)));
    assert!(!screen(&mut app).contains("this help"));
    assert_eq!(
        app.event(Event::Input(Input::Key(Key::Char('q'), Mods::CTRL))),
        Some(Action::Quit)
    );
}

/// A tree of the given files, all counted, `width` columns wide: the
/// panel's columns of the screen.
fn tree_panel(width: usize, files: &[(&str, usize, usize)]) -> String {
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "range".to_owned(),
        None,
        (width, 11),
        Indicators::Classic,
    );
    for (path, _, _) in files {
        app.event(Event::Enumerated(diff(path).change));
    }
    app.event(Event::EnumerationDone(None));
    let counts = files
        .iter()
        .enumerate()
        .map(|(i, &(_, added, removed))| (i, app.version(i), Counts { added, removed }))
        .collect();
    app.event(Event::Diffed(Done::Counts { counts }));
    if width < SIDE_TREE_WIDTH {
        app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
    }
    let columns = app.tree_columns().expect("the tree shows");
    // Between the title row and the status row.
    screen(&mut app)
        .lines()
        .skip(1)
        .take(9)
        .map(|l| {
            l.chars()
                .take(columns)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const FILES: &[(&str, usize, usize)] = &[
    (".clang-format", 12, 3),
    ("Documentation/admin-guide/kernel-parameters.txt", 1234, 56),
    (
        "drivers/net/wireless/realtek/rtw89/rtw8852c_table.c",
        54321,
        12345,
    ),
    ("kernel/sched/fair.c", 98765, 43210),
];

/// The tree takes just over a quarter of a wide screen (all of a
/// narrow one, over the diff): short names show whole, and long names
/// are cut before the counts column, whose counts stay whole.
#[test]
fn the_tree_widens_with_the_screen_and_keeps_counts_whole() {
    insta::assert_snapshot!(tree_panel(80, FILES), @r"
            ┌ Files · 4 +154332 -55614 ────────────────────────────────────────────────────┐
            │ M .clang-format                                                   +12     -3 │
            │ ▼ Documentation/admin-guide/                                                 │
            │   M kernel-parameters.txt                                       +1234    -56 │
            │ ▼ drivers/net/wireless/realtek/rtw89/                                        │
            │   M rtw8852c_table.c                                           +54321 -12345 │
            │ ▼ kernel/sched/                                                              │
            │   M fair.c                                                     +98765 -43210 │
            └«─────────────────────────────────────────────────────────────────────────────┘
        ");
    insta::assert_snapshot!(tree_panel(120, FILES), @r"
        ┌ Files · 4 +154332 -55614 ─────┐
        │ M .clang-format    +12     -3 │
        │ ▼ Documentation/admin-guide/  │
        │   M …meters.txt  +1234    -56 │
        │ ▼ …et/wireless/realtek/rtw89/ │
        │   M …2c_table.c +54321 -12345 │
        │ ▼ kernel/sched/               │
        │   M fair.c      +98765 -43210 │
        └«──────────────────────────────┘
        ");
    insta::assert_snapshot!(tree_panel(200, FILES), @r"
        ┌ Files · 4 +154332 -55614 ────────────────────────────┐
        │ M .clang-format                           +12     -3 │
        │ ▼ Documentation/admin-guide/                         │
        │   M kernel-parameters.txt               +1234    -56 │
        │ ▼ drivers/net/wireless/realtek/rtw89/                │
        │   M rtw8852c_table.c                   +54321 -12345 │
        │ ▼ kernel/sched/                                      │
        │   M fair.c                             +98765 -43210 │
        └«─────────────────────────────────────────────────────┘
        ");
}

/// Additions only, short and long names at several depths: the
/// counts stand in one column, the long names cut before it.
#[test]
fn tree_counts_stand_in_one_column() {
    let files = [
        ("README.md", 3, 0),
        (
            "crates/dffy/src/tui/snapshots/dffy__tui__app__tests__a_very_long_name.snap",
            127,
            0,
        ),
        ("crates/dffy/src/tui/app.rs", 12, 0),
        ("x", 1, 0),
    ];
    insta::assert_snapshot!(tree_panel(120, &files));
}

/// A viewer of a piped diff, read whole.
fn piped_app(input: &[u8], width: usize, height: usize) -> App {
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "stdin".to_owned(),
        None,
        (width, height),
        Indicators::Classic,
    );
    app.set_piped();
    for item in piped::items(input) {
        app.event(Event::Piped(item));
    }
    app.event(Event::EnumerationDone(None));
    app
}

/// Does the workers' diffing of piped files, on this thread.
fn read_back(app: &mut App) {
    app.draw();
    for job in app.jobs() {
        if let Job::Diff {
            file,
            version,
            change,
            piped: Some(piped),
            ..
        } = job
        {
            let loaded = piped::read(&piped, change).expect("in memory");
            app.event(Event::Diffed(Done::Diff {
                placed: None,
                file,
                version,
                result: Ok(Box::new(loaded)),
            }));
        }
    }
}

const LOG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/git-log-color.diff"
));

/// `git log -p` in the viewer: each commit a top-level node of the tree
/// with its files beneath, its header and file list as its view, and
/// `n` going on from one commit's changes into the next's.
#[test]
fn a_piped_log_shows_commits_in_the_tree_and_their_headers() {
    let mut app = piped_app(LOG, 120, 24);
    read_back(&mut app);
    insta::assert_snapshot!("piped_log_commit", screen(&mut app));
    key(&mut app, 'n');
    read_back(&mut app);
    insta::assert_snapshot!("piped_log_file", screen(&mut app));
    key(&mut app, 'n');
    read_back(&mut app);
    let shown = status(&mut app);
    assert!(
        shown.contains(" commit 2/2 ") && shown.contains(" 1/7 · "),
        "{shown}"
    );
    assert!(
        shown.contains("→ commit 2/2: 80f116f Widen calc"),
        "{shown}"
    );
    // The unchanged lines between hunks aren't in the input.
    key(&mut app, 'x');
    assert!(
        screen(&mut app).contains("a piped diff has only its hunks"),
        "{}",
        screen(&mut app)
    );
    // Collapsing the first commit hides its file in the tree.
    app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
    key(&mut app, 'g');
    key(&mut app, 'h');
    let shown = screen(&mut app);
    let rows: Vec<&str> = shown.lines().collect();
    assert!(rows[1].contains("▶ 1fcf726 Add a third line"), "{shown}");
    assert!(rows[2].contains("▼ 80f116f Widen calc"), "{shown}");
}

/// `dffy show`: the commit's files are counted in the background, and
/// its header gives their total only once every one is counted, never
/// the zeros or partial sum it has before.
#[test]
fn a_commit_header_waits_for_every_files_counts() {
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "show HEAD".to_owned(),
        None,
        (100, 20),
        Indicators::Classic,
    );
    app.event(Event::Piped(piped::Item::Commit(Arc::new(
        dffy_core::patch::Commit {
            id: Some("0123abc".repeat(5) + "01234"),
            subject: "Change two files".to_owned(),
            lines: vec![(
                dffy_core::patch::TextKind::CommitMessage,
                "    Change two files".into(),
            )],
        },
    ))));
    for path in ["a.rs", "b.rs"] {
        app.event(Event::Enumerated(diff(path).change));
    }
    app.event(Event::EnumerationDone(None));
    let shows = |app: &mut App, summary: &str| {
        let shown = screen(app);
        assert!(shown.contains(summary), "{shown}");
    };
    shows(&mut app, "2 files …");
    let count = |added, removed| Counts { added, removed };
    app.event(Event::Diffed(Done::Counts {
        counts: vec![(1, app.version(1), count(3, 1))],
    }));
    shows(&mut app, "2 files …");
    app.event(Event::Diffed(Done::Counts {
        counts: vec![(2, app.version(2), count(2, 0))],
    }));
    shows(&mut app, "2 files +5 -1");
}

/// `u` from the diff selects the folder of the file shown; each `u`
/// after climbs one more.
#[test]
fn u_climbs_the_tree_a_folder_at_a_time() {
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "range".to_owned(),
        None,
        (120, 20),
        Indicators::Classic,
    );
    for path in [
        "README.md",
        "src/app/main.rs",
        "src/app/view.rs",
        "src/lib.rs",
    ] {
        app.event(Event::Enumerated(diff(path).change));
    }
    app.event(Event::EnumerationDone(None));
    key(&mut app, 'f');
    key(&mut app, 'u');
    key(&mut app, 'h');
    assert!(screen(&mut app).contains("▶ app/"), "{}", screen(&mut app));
    key(&mut app, 'l');
    key(&mut app, 'u');
    key(&mut app, 'u');
    key(&mut app, 'h');
    let shown = screen(&mut app);
    assert!(shown.contains("▶ src/"), "{shown}");
    assert!(!shown.contains("▼ app/"), "{shown}");
}

/// Lines of a combined diff longer than the screen end in `…` and
/// scroll sideways like code.
#[test]
fn a_long_combined_line_is_cut_and_scrolls() {
    let long = format!("++{}END", "x".repeat(150));
    let input = format!(
        "diff --cc f.txt\nindex 1,2..3\n--- a/f.txt\n+++ b/f.txt\n@@@ -1,1 -1,1 +1,1 @@@\n- a\n -b\n{long}\n"
    );
    let mut app = piped_app(input.as_bytes(), 80, 12);
    let row = |app: &mut App| {
        screen(app)
            .lines()
            .find(|l| l.contains("xxx"))
            .map(str::to_owned)
            .unwrap_or_default()
    };
    let before = row(&mut app);
    assert!(before.ends_with('…') && !before.contains("END"), "{before}");
    for _ in 0..10 {
        key(&mut app, '>');
    }
    let after = row(&mut app);
    assert!(after.starts_with('…') && after.contains("END"), "{after}");
}

/// A viewer of `FILES`, all counted.
fn files_app(width: usize, height: usize) -> App {
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "range".to_owned(),
        None,
        (width, height),
        Indicators::Classic,
    );
    for (path, _, _) in FILES {
        app.event(Event::Enumerated(diff(path).change));
    }
    app.event(Event::EnumerationDone(None));
    let counts = FILES
        .iter()
        .enumerate()
        .map(|(i, &(_, added, removed))| (i, app.version(i), Counts { added, removed }))
        .collect();
    app.event(Event::Diffed(Done::Counts { counts }));
    app
}

/// The count pass starts with the files on the tree's screen, then a
/// screen's worth below and above them, and starts again with what's on
/// screen when the tree scrolls to files not counted yet.
#[test]
fn the_count_pass_starts_where_the_tree_shows() {
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "range".to_owned(),
        None,
        (200, 12),
        Indicators::Classic,
    );
    for i in 0..80 {
        app.event(Event::Enumerated(diff(&format!("f{i:02}")).change));
    }
    app.event(Event::EnumerationDone(None));
    screen(&mut app);
    app.tree.scroll_by(20);
    screen(&mut app);
    let shown: Vec<usize> = app.tree.files_on_screen().collect();
    let (top, height) = (shown[0], shown.len());
    let pass = |app: &mut App| -> Option<Vec<usize>> {
        Some(app.take_count_pass()?.iter().map(|f| f.0).collect())
    };
    let first = pass(&mut app).expect("a pass once the list is complete");
    let near: Vec<usize> = (top..top + 2 * height).chain(top - height..top).collect();
    assert_eq!(first[..near.len()], near[..]);
    assert_eq!(first.len(), 80);
    assert_eq!(pass(&mut app), None, "started again unasked");

    let counts = near
        .iter()
        .map(|&f| (f, app.version(f), Counts::default()))
        .collect();
    app.event(Event::Diffed(Done::Counts { counts }));
    app.tree.scroll_by(height.cast_signed());
    screen(&mut app);
    assert_eq!(pass(&mut app), None, "scrolled to files already counted");
    app.tree.scroll_by((2 * height).cast_signed());
    screen(&mut app);
    let shown: Vec<usize> = app.tree.files_on_screen().collect();
    let again = pass(&mut app).expect("scrolled to files not counted");
    assert_eq!(again[..shown.len()], shown[..]);
    assert_eq!(again.len(), 80 - near.len());
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        key(app, c);
    }
}

/// The picker's rows as markup: matched characters are `<hint>`.
fn picker_markup(app: &mut App) -> String {
    let (width, height) = (app.screen.width(), app.screen.height());
    let picker = app.picker.as_mut().expect("the picker is open");
    picker
        .rows(
            app.doc.document.files(),
            app.doc.document.commits(),
            (width, height - 1),
            picker::Progress::Done,
        )
        .iter()
        .map(|(c, selected)| {
            let mark = if *selected { ">" } else { " " };
            format!("{mark}{}", c.markup().trim_end())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `o` lists every file; typing narrows it, file names first, marking
/// what matched; `j` is typed, not a move; `enter` opens the pick.
#[test]
fn the_picker_filters_as_you_type_and_opens_the_pick() {
    let mut app = files_app(60, 10);
    key(&mut app, 'o');
    insta::assert_snapshot!(screen(&mut app), @r"
        > ▏ type to filter · enter open · esc close        4 files
        M .clang-format                                  +12     -3
        M …ntation/admin-guide/kernel-parameters.txt   +1234    -56
        M …t/wireless/realtek/rtw89/rtw8852c_table.c  +54321 -12345
        M kernel/sched/fair.c                         +98765 -43210




          range   unified         / search · tab tree · ␣ commands
        ");
    typed(&mut app, "fair");
    insta::assert_snapshot!(picker_markup(&mut app), @r"
         <hint> > </>fair<hint>▏</>                                          <dim>1/4 files</>
        > <warn>M</> kernel/sched/<hint>fair</>.c                         <added>+98765</> <removed>-43210</>
        ");
    typed(&mut app, "j");
    insta::assert_snapshot!(picker_markup(&mut app), @r"
         <hint> > </>fairj<hint>▏</>                                         <dim>0/4 files</>
         <dim>   no file matches</>
        ");
    app.event(Event::Input(Input::Key(Key::Backspace, Mods::NONE)));
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    assert!(app.picker.is_none());
    assert_eq!(app.doc.document.pos.file, 3, "kernel/sched/fair.c");
    // A tap on a row opens that file.
    key(&mut app, 'o');
    app.draw();
    app.event(Event::Input(Input::Click { x: 10, y: 1 }));
    assert!(app.picker.is_none());
    assert_eq!(app.doc.document.pos.file, 0);
}

/// Under 100 columns the title is `‹ i/N name ›`: taps on the arrows
/// move between files, a tap on the name opens the picker, each at its
/// exact columns.
#[test]
fn the_narrow_title_switches_files_with_taps() {
    let mut app = files_app(50, 10);
    let title = |app: &mut App| screen(app).lines().next().unwrap_or_default().to_owned();
    insta::assert_snapshot!(title(&mut app), @" ‹ 1/4 .clang-format › +12 -3");
    // The ranges as last drawn: the name's width moves the `›`.
    let hits = |app: &App| app.switcher.clone().expect("drawn").1;
    let tap = |app: &mut App, x: usize| {
        app.event(Event::Input(Input::Click {
            x: u16::try_from(x).expect("column"),
            y: 0,
        }));
        app.draw();
    };
    // Both cells beside an arrow are its padding: still the arrow.
    let [_, _, next] = hits(&app);
    tap(&mut app, next.start);
    assert_eq!(app.doc.document.pos.file, 1);
    let [_, _, next] = hits(&app);
    tap(&mut app, next.end - 1);
    assert_eq!(app.doc.document.pos.file, 2);
    let [prev, _, _] = hits(&app);
    tap(&mut app, prev.start);
    assert_eq!(app.doc.document.pos.file, 1);
    insta::assert_snapshot!(title(&mut app), @" ‹ 2/4 kernel-parameters.txt › +1234 -56");
    let [_, name, next] = hits(&app);
    tap(&mut app, next.end);
    assert!(app.picker.is_none(), "past the arrow");
    tap(&mut app, name.start);
    assert!(app.picker.is_some());
}

/// On a narrow screen the tree covers the whole diff area. A tap on a
/// folder row toggles it, a tap on a file row opens the file and closes
/// the tree, and the wheel over it scrolls the tree, not the diff.
#[test]
fn taps_and_the_wheel_on_the_narrow_tree_act_on_the_row_under_them() {
    let mut app = files_app(60, 7);
    key(&mut app, 't');
    let shown = screen(&mut app);
    let rows: Vec<&str> = shown.lines().collect();
    assert!(rows[1].starts_with("┌ Files · 4"), "{shown}");
    // Down to the status row, which stays.
    assert!(
        rows[2..6]
            .iter()
            .all(|r| r.starts_with('│') || r.starts_with('└')),
        "{shown}"
    );
    // The panel shows 3 rows of 7; the wheel scrolls it alone, 3 rows
    // a notch.
    let pos = app.doc.document.pos;
    app.event(Event::Input(Input::Scroll {
        delta: 1,
        x: 10,
        y: 3,
    }));
    let scrolled = screen(&mut app);
    assert_eq!(app.doc.document.pos, pos);
    assert!(
        scrolled
            .lines()
            .nth(2)
            .is_some_and(|r| r.contains("rtw89/")),
        "{scrolled}"
    );
    app.event(Event::Input(Input::Scroll {
        delta: -1,
        x: 10,
        y: 3,
    }));
    // Row 2 is the first row inside the border: a folder; its last row,
    // the bottom edge of the panel, is the border, which does nothing.
    app.event(Event::Input(Input::Click { x: 10, y: 3 }));
    let shown = screen(&mut app);
    assert!(shown.contains("▶ Documentation/admin-guide/"), "{shown}");
    app.event(Event::Input(Input::Click { x: 10, y: 5 }));
    assert!(app.tree_shown, "the border isn't a row");
    app.event(Event::Input(Input::Click { x: 10, y: 3 }));
    assert!(screen(&mut app).contains("▼ Documentation/admin-guide/"));
    // The first row: `.clang-format`, a file.
    app.event(Event::Input(Input::Click { x: 3, y: 2 }));
    assert!(!app.tree_shown, "a picked file closes the tree");
    assert_eq!(app.doc.document.pos.file, 0);
}

#[test]
fn plus_and_minus_in_the_tree_widen_and_narrow_it() {
    let mut app = app(120, 20);
    let width = app.side_width();
    app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
    key(&mut app, '+');
    assert_eq!(app.side_width(), width.map(|w| w + TREE_STEP));
    for _ in 0..40 {
        key(&mut app, '+');
    }
    assert_eq!(app.side_width(), Some(120 - MIN_DIFF));
    for _ in 0..40 {
        key(&mut app, '-');
    }
    assert_eq!(app.side_width(), Some(MIN_TREE));
    assert!(
        screen(&mut app)
            .lines()
            .next()
            .is_some_and(|l| l.starts_with("┌ Files"))
    );
}

/// In the rendered view the title counts what the tree counts: the
/// file's changed source lines, not its changed rendered lines.
#[test]
fn the_rendered_view_titles_the_trees_counts() {
    let side = Some(Side::Worktree {
        mode: Mode::File,
        id: None,
    });
    let old = Text::new("Intro.\n\nGone para here.\n\nBeta three four.\n\nTail.\n".into());
    let new = Text::new("Intro.\n\nBeta three FOUR.\n\nTail.\n".into());
    let fd = FileDiff {
        change: FileChange {
            path: "README.md".into(),
            status: Status::Modified,
            old: side,
            new: side,
            staging: None,
        },
        hunks: hunks(&old, &new, &Options::default()),
        old,
        new,
        binary: false,
        too_large: false,
        note: None,
    };
    let mut config = Config::default();
    config.set("render", "md:fake").expect("valid");
    let mut app = app_of(&config, "working tree", fd, (120, 20));
    let para = |source: usize, text: &str| plugin::Block {
        kind: "paragraph".to_owned(),
        source: source..source + 1,
        lines: [text, ""]
            .map(|text| plugin::Line {
                text: text.to_owned(),
                paint: Vec::new(),
                links: Vec::new(),
            })
            .to_vec(),
        language: None,
    };
    let sides = [
        plugin::Side {
            blocks: vec![
                para(0, "Intro."),
                para(2, "Gone para here."),
                para(4, "Beta three four."),
                para(6, "Tail."),
            ],
        },
        plugin::Side {
            blocks: vec![
                para(0, "Intro."),
                para(2, "Beta three FOUR."),
                para(4, "Tail."),
            ],
        },
    ];
    let diff = Arc::clone(app.doc.document.diff_of(0).expect("diffed"));
    let rendered = view::rendered::build(Arc::clone(&diff), sides, crate::grammars(), 3, 60);
    assert_eq!(
        rendered.counts(),
        Counts {
            added: 1,
            removed: 3
        }
    );
    app.event(Event::Diffed(Done::Rendered {
        file: 0,
        version: app.version(0),
        id: app.render_id,
        diff,
        result: Ok(Box::new(rendered)),
    }));
    app.doc.set_rendered_mode(true);
    let shown = screen(&mut app);
    assert!(shown.contains("Beta three FOUR."), "{shown}");
    let title = shown.lines().next().expect("the title row");
    let tree = shown
        .lines()
        .find(|l| l.contains("README.md") && l.contains('│'))
        .expect("the tree's row");
    assert!(title.contains("+1 -3"), "{title}");
    assert!(tree.contains("+1") && tree.contains("-3"), "{tree}");
}

/// A `render` setting alone looks nothing up: drawing and moving
/// between files of the renderer's type ask for no program, and no
/// render job is queued, until `r` asks for the rendered view.
#[test]
fn renderers_are_looked_up_only_when_r_asks() {
    let mut config = Config::default();
    config.set("render", "rs:nothere").expect("valid");
    let mut app = app_of(&config, "working tree", diff("src/a.rs"), (120, 20));
    for c in ['f', 'F', 'n', 'N', 'j'] {
        screen(&mut app);
        key(&mut app, c);
    }
    screen(&mut app);
    assert_eq!(app.programs.len(), 0);
    assert!(!app.jobs().iter().any(|j| matches!(j, Job::Render(_))));
    key(&mut app, 'r');
    assert_eq!(app.programs.len(), 1, "looked up once r asked");
    assert!(!app.doc.document.rendered(), "not on without the program");
}

/// Wrapped, a file left part way down a long row comes back at that
/// same screen row of it, not at the row's first.
#[test]
fn a_place_inside_a_wrapped_row_is_kept_across_files() {
    let old = (1..=60)
        .map(|i| format!("line {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    let long = "a line long enough to wrap ".repeat(10);
    let new = old.replace("line 4\n", &format!("line 4 {long}\n"));
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    let fd = FileDiff {
        hunks: hunks(&old, &new, &Options::default()),
        old,
        new,
        ..diff("src/a.rs")
    };
    let mut config = Config::default();
    config.apply_source("wrap = true\n", &mut Vec::new());
    let mut app = app_of(&config, "working tree", fd, (60, 10));
    for _ in 0..20 {
        if app.doc.document.pos.offset > 0 {
            break;
        }
        key(&mut app, 'j');
    }
    assert!(
        app.doc.document.pos.offset > 0,
        "{:?}",
        app.doc.document.pos
    );
    let (pos, before) = (app.doc.document.pos, screen(&mut app));
    key(&mut app, 'j');
    assert_ne!(app.doc.document.pos, pos, "not at the bottom");
    key(&mut app, 'k');
    key(&mut app, 'f');
    assert_eq!(app.doc.document.pos.file, 1);
    key(&mut app, 'F');
    assert_eq!(app.doc.document.pos, pos);
    let after = screen(&mut app);
    assert_eq!(after, before);
    insta::assert_snapshot!("wrapped_place_kept", after);
}

/// Thirty C functions, the 15th and 16th swapped: lines removed and
/// added to the line diff, code moved to the syntax diff.
fn moved_diff() -> FileDiff {
    let f = |i: usize| format!("int f{i}(void) {{ return {i}; }}\n");
    let old: String = (1..=30).map(f).collect();
    let new: String = (1..=30)
        .map(|i| match i {
            15 => f(16),
            16 => f(15),
            _ => f(i),
        })
        .collect();
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    FileDiff {
        hunks: hunks(&old, &new, &Options::default()),
        old,
        new,
        ..diff("src/f.c")
    }
}

/// `d` between the line diff and the syntax diff keeps the view where
/// it was in the file's rows.
#[test]
fn the_syntax_diff_toggle_keeps_the_place() {
    let mut app = app_of(&Config::default(), "working tree", moved_diff(), (100, 10));
    for _ in 0..4 {
        key(&mut app, 'j');
    }
    let lines = screen(&mut app);
    key(&mut app, 'd');
    assert!(app.doc.document.structural);
    app.draw();
    for job in app.jobs() {
        if let Job::Structural {
            file,
            version,
            diff,
        } = job
        {
            app.event(Event::Diffed(Done::Structural {
                file,
                version,
                result: File::prepare(crate::grammars(), None).structural(&diff),
                diff,
            }));
        }
    }
    let syntax = screen(&mut app);
    assert!(syntax.contains("»│"), "the syntax diff shows:\n{syntax}");
    insta::assert_snapshot!("syntax_diff_on", syntax);
    key(&mut app, 'd');
    let off = screen(&mut app);
    assert_eq!(off, lines);
    insta::assert_snapshot!("syntax_diff_off", off);
}

/// The screen's first `width` columns with each run of cells in one of
/// `roles`' styles in `<role>…</>`.
fn role_markup(app: &mut App, roles: &[Role], width: usize) -> String {
    let styles: Vec<_> = (roles.iter())
        .map(|&r| (app.theme.style(r), r.key().to_owned()))
        .collect();
    style_markup(app, &styles, width)
}

/// [`role_markup`] for any styles, each in `<label>…</>`.
fn style_markup(app: &mut App, styles: &[(Style, String)], width: usize) -> String {
    app.draw();
    let grid = app.screen.shown_grid();
    (0..grid.height())
        .map(|y| {
            let mut line = String::new();
            let mut current = None;
            for cell in grid.row(y).iter().take(width) {
                let role = styles
                    .iter()
                    .find(|(style, _)| *style == cell.style())
                    .map(|(_, label)| label.as_str());
                if role != current {
                    if current.is_some() {
                        line.push_str("</>");
                    }
                    if let Some(label) = role {
                        line.extend(["<", label, ">"]);
                    }
                    current = role;
                }
                line.push_str(cell.text());
            }
            if current.is_some() {
                line.push_str("</>");
            }
            line.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `git show` of a merge in the viewer: the commit's header, metadata
/// and message in their colors, then the combined diff's lines colored
/// by their prefix columns.
#[test]
fn a_piped_merge_shows_its_commit_and_combined_diff_in_their_colors() {
    let merge = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/git-show-merge.diff"
    ));
    let roles = [
        Role::CommitHeader,
        Role::Dim,
        Role::HunkHeader,
        Role::Added,
        Role::Removed,
    ];
    let mut app = piped_app(merge, 100, 14);
    // The default theme shares colors between these roles and others
    // (`warn` is yellow too): underlined, each is told apart.
    app.theme = Theme::parse(
        "roles",
        "commit-header = yellow underline\ndim = dim underline\n\
             hunk-header = cyan underline\nadded = green underline\nremoved = red underline\n",
        &mut Vec::new(),
    );
    key(&mut app, 't');
    read_back(&mut app);
    insta::assert_snapshot!(
        "piped_merge_commit",
        role_markup(&mut app, &roles, usize::MAX)
    );
    key(&mut app, 'f');
    read_back(&mut app);
    insta::assert_snapshot!(
        "piped_merge_combined",
        role_markup(&mut app, &roles, usize::MAX)
    );
}

/// `view = auto` splits from `split-min-width` columns of screen, 140
/// by default, and is unified a column short of it.
#[test]
fn auto_splits_from_the_split_min_width() {
    let mut app = app(140, 12);
    assert!(app.doc.layout.split);
    let split = screen(&mut app);
    assert!(
        split.lines().any(|l| l.matches("line 4").count() == 2),
        "side by side:\n{split}"
    );
    insta::assert_snapshot!("auto_at_split_min_width", split);
    app.resize(139, 12);
    assert!(!app.doc.layout.split);
    insta::assert_snapshot!("auto_below_split_min_width", screen(&mut app));
}

const FILE: &str = "src/some/deeply/nested/module/file.rs";

/// A draft thread with a reply on new line 2, a posted comment on new
/// line 50, a resolved one on removed old line 4, a comment on the
/// whole file, one on a file not in the diff, and one on `far` whose
/// lines are gone.
fn review_json(far: &str) -> String {
    let line = |id: &str, path: &str, side: &str, n: usize, text: &str, extra: &str| {
        let numbers = if side == "old" {
            format!("{{\"old\":{n},\"new\":null}}")
        } else {
            format!("{{\"old\":null,\"new\":{n}}}")
        };
        format!(
            r#"{{"id":"{id}","path":"{path}","side":"{side}","start":{numbers},"end":{numbers},"text":"{text}","author":"","created":"2026-11-17T10:00:00Z","anchor":{{"before":[],"lines":["{}"],"after":[]}}{extra}}}"#,
            match id {
                "d1" => "line 2",
                "p1" => "line fifty",
                "r1" => "line 4",
                _ => "not there",
            }
        )
    };
    let comments = [
            line("d1", FILE, "new", 2, "Why two?", ""),
            r#"{"id":"d2","path":"src/some/deeply/nested/module/file.rs","text":"Because.","author":"","created":"2026-11-17T11:00:00Z","in_reply_to":"d1"}"#.to_owned(),
            line(
                "p1",
                FILE,
                "new",
                50,
                "Fifty?",
                r#","ref":"github:1","author":"octocat","url":"https://github.com/o/r/pull/1#discussion_r1","outdated":true"#,
            ),
            line("r1", FILE, "old", 4, "Old four", r#","ref":"github:2","resolved":true"#),
            r#"{"id":"f1","path":"src/some/deeply/nested/module/file.rs","text":"On the whole file","created":"2026-11-17T09:00:00Z"}"#.to_owned(),
            line("g1", "gone.rs", "new", 1, "Gone", ""),
            line("m1", far, "new", 10, "Moved away", ""),
        ];
    format!("{{\"version\":1,\"comments\":[{}]}}", comments.join(","))
}

/// The viewer of [`app`] with ten more files, taking comments from a
/// file in `dir` written with [`review_json`], drawn once they're
/// placed.
fn commented(dir: &tempfile::TempDir) -> (App, PathBuf) {
    let mut app = app(100, 20);
    for i in 0..10 {
        app.event(Event::Enumerated(diff(&format!("src/zz{i}.rs")).change));
    }
    let path = dir.path().join("comments.json");
    std::fs::write(&path, review_json("src/zz9.rs")).expect("write");
    app.set_comments(path.clone());
    screen(&mut app);
    (app, path)
}

fn read_comments(path: &Path) -> review::Review {
    review::read(path).expect("reads")
}

fn press(app: &mut App, key: Key) -> Option<Action> {
    app.event(Event::Input(Input::Key(key, Mods::NONE)))
}

/// `c` on a selection opens a text box right under it, where every key
/// types, `enter` and a paste included; `esc` saves the text on the
/// selected lines with their anchor and distance, and the new thread
/// shows there, focused, with its mark in the gutter.
#[test]
fn c_writes_a_comment_in_a_box_under_the_selection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("dffy").join("comments.json");
    let mut app = app(100, 20);
    app.set_comments(path.clone());
    screen(&mut app);
    key(&mut app, 'v');
    key(&mut app, 'j');
    assert_eq!(press(&mut app, Key::Char('c')), None);
    assert!(app.doc.selection.is_none());
    typed(&mut app, "Why these? q");
    press(&mut app, Key::Backspace);
    press(&mut app, Key::Enter);
    app.event(Event::Input(Input::Paste("Pasted\r\nlines".to_owned())));
    insta::assert_snapshot!("comment_box", screen(&mut app));
    assert!(!path.exists(), "nothing saved while writing");
    press(&mut app, Key::Esc);
    assert!(app.doc.writing().is_none());
    let saved = read_comments(&path);
    let [comment] = saved.comments.as_slice() else {
        panic!("one comment: {saved:?}");
    };
    assert_eq!(
        (comment.path.as_str(), comment.text.as_str(), comment.side),
        (FILE, "Why these? \nPasted\nlines", review::Side::New)
    );
    assert_eq!(
        (comment.start, comment.end),
        (
            Some(review::LineNumbers {
                old: Some(0),
                new: Some(0)
            }),
            Some(review::LineNumbers {
                old: Some(1),
                new: Some(1)
            })
        )
    );
    let anchor = comment.anchor.as_ref().expect("an anchor");
    assert_eq!(anchor.lines, ["line 1", "line 2"]);
    assert_eq!(anchor.after, ["line 3", "line four"]);
    assert_eq!(comment.distance, Some(2));
    let shown = screen(&mut app);
    // Outside a review session nothing is posted, so nothing's a draft.
    let rows: Vec<&str> = shown.lines().filter(|l| l.contains('●')).collect();
    assert_eq!(rows.len(), 2, "{shown}");
    assert!(rows[0].contains("line 1") && rows[1].contains("line 2"));
    let below = shown
        .lines()
        .skip_while(|l| !l.contains("line 2"))
        .nth(1)
        .unwrap_or_default();
    assert!(below.contains("▾ you · just now ━"), "{shown}");
    assert_eq!(
        app.doc.focused().map(|t| t.root.as_str()),
        Some(comment.id.as_str())
    );

    // An empty box saves nothing.
    press(&mut app, Key::Esc);
    key(&mut app, 'c');
    typed(&mut app, "  ");
    press(&mut app, Key::Esc);
    assert!(screen(&mut app).contains("empty; nothing saved"));
    assert_eq!(read_comments(&path).comments.len(), 1);

    // Editing a draft down to nothing deletes it.
    key(&mut app, 'm');
    key(&mut app, 'e');
    for _ in 0..200 {
        press(&mut app, Key::Backspace);
    }
    press(&mut app, Key::Esc);
    assert!(screen(&mut app).contains("draft deleted"));
    assert_eq!(read_comments(&path).comments.len(), 0);
}

/// A `d` armed for one draft doesn't outlast the mouse: after a click
/// on another thread, `d` asks again rather than deleting it.
#[test]
fn a_click_disarms_d() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    let shown = screen(&mut app);
    let y = shown
        .lines()
        .position(|l| l.contains("Why two?"))
        .expect("the draft on screen");
    key(&mut app, 'm');
    assert_eq!(focused(&app), Some("f1"));
    key(&mut app, 'd');
    let y = u16::try_from(y).expect("on screen");
    app.event(Event::Input(Input::Click { x: 60, y }));
    app.event(Event::Input(Input::Release { x: 60, y }));
    assert_eq!(focused(&app), Some("d1"));
    key(&mut app, 'd');
    assert_eq!(read_comments(&path).comments.len(), 7, "nothing deleted");
    assert!(status(&mut app).contains("d again deletes"));
}

/// `ctrl+g` hands the box's text to the editor through the compose
/// path; what's written there is saved. When the editor fails, the box
/// opens again with the text. `ctrl+e`, which Ghostty sends for
/// cmd+right, stays in the box.
#[test]
fn ctrl_g_continues_the_box_in_the_editor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = app(100, 20);
    app.set_comments(path.clone());
    screen(&mut app);
    key(&mut app, 'c');
    typed(&mut app, "Draft");
    let ctrl = |c| Event::Input(Input::Key(Key::Char(c), Mods::CTRL));
    assert_eq!(app.event(ctrl('e')), None);
    assert!(app.doc.writing().is_some());
    assert_eq!(
        app.event(ctrl('g')),
        Some(Action::Compose("Draft".to_owned()))
    );
    assert!(app.doc.writing().is_none());
    app.composed(None);
    let reopened = app.doc.writing().map(|w| w.text.text().to_owned());
    assert_eq!(reopened.as_deref(), Some("Draft"));
    assert!(app.event(ctrl('g')).is_some());
    app.composed(Some("Written in vim\n".to_owned()));
    let saved = read_comments(&path);
    assert_eq!(saved.comments.len(), 1);
    assert_eq!(saved.comments[0].text, "Written in vim");
    // The command panel's entry takes the focused thread's draft there,
    // once it's placed.
    screen(&mut app);
    key(&mut app, ' ');
    assert_eq!(
        press(&mut app, Key::Char('E')),
        Some(Action::Compose("Written in vim".to_owned()))
    );
    app.composed(Some("Edited".to_owned()));
    assert_eq!(read_comments(&path).comments[0].text, "Edited");
}

/// When saving the text box fails, it opens again with the text, and a
/// second `esc` discards it rather than trying again.
#[test]
fn a_second_esc_discards_a_box_that_failed_to_save() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("file"), "").expect("write");
    let mut app = app(160, 20);
    app.set_comments(dir.path().join("file/comments.json"));
    screen(&mut app);
    key(&mut app, 'c');
    typed(&mut app, "Kept");
    press(&mut app, Key::Esc);
    assert!(app.doc.writing().is_some(), "open again");
    let shown = status(&mut app);
    assert!(shown.contains("esc again discards"), "{shown}");
    press(&mut app, Key::Esc);
    assert!(app.doc.writing().is_none());
    assert!(status(&mut app).contains("discarded"));
}

const BASE: &str = "aaaa111";
const HEAD: &str = "bbbb222";

/// A plugin that calls a review `PR #7` and posts it with four events,
/// the first of which leaves it open on the host.
const DESCRIBED: &str = r##"{"version":1,"command":"pr","noun":"pull request","nouns":"pull requests","short":"PR","id_prefix":"#","events":[{"id":"pending","key":"p","label":"pending: on the host, for you only","done":"review pending","submits":false},{"id":"comment","key":"c","label":"comment","done":"commented"},{"id":"approve","key":"a","label":"approve","done":"approved"},{"id":"request_changes","key":"r","label":"request changes","done":"changes requested"}]}"##;

fn described(app: &mut App, answer: &str) {
    let answer = plugin::json::parse(answer.as_bytes()).expect("JSON");
    app.set_naming(Some(Naming::decode(&answer).expect("decodes")));
}

fn session(dir: &tempfile::TempDir, head: &str, last: Option<&str>) -> Session {
    Session {
        id: "7".to_owned(),
        label: None,
        title: Some("Spell out five".to_owned()),
        url: Some("https://github.com/acme/widgets/pull/7".to_owned()),
        base: BASE.to_owned(),
        head: head.to_owned(),
        last_reviewed_head: last.map(str::to_owned),
        state: None,
        labels: Vec::new(),
        comments: dir.path().join("comments.json"),
    }
}

fn said(app: &mut App, id: u64, said: Said) -> Option<Action> {
    app.event(Event::Review { id, said })
}

/// The request asked for, with its id.
fn plugin_run(action: Option<Action>) -> (u64, Request) {
    match action {
        Some(Action::Review { id, request }) => (id, request),
        other => panic!("not a request: {other:?}"),
    }
}

fn prepare(id: &str) -> Request {
    Request::Prepare { id: id.to_owned() }
}

/// A dry run, with the local review's `drafts` and the roots in view.
fn dry_run(drafts: Option<(&Path, &[&str])>) -> Request {
    Request::DryRun {
        id: "7".to_owned(),
        drafts: drafts.map(|(file, roots)| {
            let roots = roots.iter().map(|&r| r.to_owned()).collect();
            (file.to_owned(), roots)
        }),
    }
}

fn post(event: &str, summary: Option<&str>) -> Request {
    Request::Post {
        id: "7".to_owned(),
        event: event.to_owned(),
        labels: Vec::new(),
        summary: summary.map(str::to_owned),
    }
}

fn range(action: Option<Action>) -> (String, Option<PathBuf>, Option<(String, String)>) {
    match action {
        Some(Action::Retarget {
            revisions,
            comments,
            moved,
        }) => (revisions.expect("a range").revs.join(" "), comments, moved),
        other => panic!("not a retarget: {other:?}"),
    }
}

/// The command panel, the picker, the notes and the titles say what
/// the review plugin reviews in its own words, and name a review by
/// its short form, its id prefix and its label, else its id; the id
/// is what the plugin's verbs take.
#[test]
fn review_labels_use_the_plugins_words() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    app.reviewing.plugin = Some("test".to_owned());
    described(
        &mut app,
        r#"{"version":1,"command":"mr","noun":"merge request","nouns":"merge requests","short":"MR","id_prefix":"!"}"#,
    );
    let entry = app
        .command_entries()
        .into_iter()
        .find(|e| e.action == Act::OpenReview)
        .expect("the panel's entry");
    assert_eq!(entry.label, "Open merge request…");
    press(&mut app, Key::Char('p'));
    assert!(
        status(&mut app).contains("not on a merge request; dffy mr <id> opens one"),
        "{}",
        status(&mut app)
    );
    key(&mut app, ' ');
    let (list, _) = plugin_run(press(&mut app, Key::Char('O')));
    assert!(status(&mut app).contains("listing merge requests…"));
    said(
        &mut app,
        list,
        Said::Listed(vec![Listed {
            id: "I8f2c".to_owned(),
            label: Some("12".to_owned()),
            title: "Cache it".to_owned(),
            author: "hubot".to_owned(),
            reason: Reason::Mine,
            group: None,
            drafts: 0,
        }]),
    );
    let picker = screen(&mut app);
    assert!(
        picker.contains(" merge requests ") && picker.contains("!12 ") && !picker.contains('#'),
        "{picker}"
    );
    let (prepare, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, self::prepare("I8f2c"));
    assert!(status(&mut app).contains("fetching MR !12…"));
    let session = Session {
        id: "I8f2c".to_owned(),
        label: Some("12".to_owned()),
        ..session(&dir, HEAD, None)
    };
    said(&mut app, prepare, Said::Session(session));
    assert!(status(&mut app).contains("MR !12 · Spell out five"));
    app.message = None;
    assert!(status(&mut app).contains("MR !12 · fetching threads"));
    assert_eq!(app.description, "MR !12");
    app.reviewing.job = None;
    let (_, request) = plugin_run(press(&mut app, Key::Char('p')));
    assert_eq!(
        request,
        Request::Pull {
            id: "I8f2c".to_owned()
        }
    );
}

/// Without a description, a review reads as one by its id, in words
/// any host's would fit.
#[test]
fn an_undescribed_plugin_reviews_reviews() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    app.reviewing.plugin = Some("test".to_owned());
    press(&mut app, Key::Char('p'));
    assert!(status(&mut app).contains("not on a review"));
    let entry = (app.command_entries().into_iter())
        .find(|e| e.action == Act::OpenReview)
        .expect("the panel's entry");
    assert_eq!(entry.label, "Open review…");
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(&mut app, prepare, Said::Progress("1/2".to_owned()));
    app.message = None;
    let shown = status(&mut app);
    assert!(shown.contains("review 7 · fetching threads 1/2"), "{shown}");
}

/// "Open pull request…" lists the PRs, grouped by why they're listed:
/// the reasons dffy knows first, then the plugin's own groups in the
/// order they came, by its titles. `enter` prepares one, whose session
/// line switches the viewer to its range and comments in place. Its
/// threads keep downloading, which the status bar shows. Runs replaced
/// since say nothing.
#[test]
fn the_pr_picker_opens_a_pr_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 30);
    described(&mut app, DESCRIBED);
    screen(&mut app);
    key(&mut app, ' ');
    assert_eq!(press(&mut app, Key::Char('O')), None);
    assert!(status(&mut app).contains("no review plugin set"));
    app.reviewing.plugin = Some("test".to_owned());
    key(&mut app, ' ');
    let (list, request) = plugin_run(press(&mut app, Key::Char('O')));
    assert_eq!(request, Request::List);
    let listed = |id: usize, reason, group: Option<&str>| Listed {
        id: id.to_string(),
        label: None,
        title: format!("Change {id}"),
        author: "hubot".to_owned(),
        reason,
        group: group.map(str::to_owned),
        drafts: usize::from(id == 9) * 2,
    };
    said(
        &mut app,
        list,
        Said::Listed(vec![
            listed(3, Reason::Other, Some("Starred")),
            listed(11, Reason::Mine, None),
            listed(5, Reason::Other, Some("Mentions you")),
            listed(9, Reason::ReviewRequested, None),
            listed(4, Reason::Other, Some("Starred")),
            listed(7, Reason::CurrentBranch, None),
        ]),
    );
    let shown = screen(&mut app);
    let at = |text: &str| {
        shown
            .find(text)
            .unwrap_or_else(|| panic!("{text}:\n{shown}"))
    };
    assert!(at("This branch") < at("#7") && at("#7") < at("Waiting on your review"));
    assert!(at("#9") < at("Yours") && at("Yours") < at("#11"));
    assert!(at("#11") < at("Starred") && at("Starred") < at("#3"));
    assert!(at("#3") < at("#4") && at("#4") < at("Mentions you"));
    assert!(at("Mentions you") < at("#5"));
    assert!(
        at("#9") < at("2 drafts") && at("2 drafts") < at("Yours"),
        "{shown}"
    );
    press(&mut app, Key::Down);
    let (prepare, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, self::prepare("9"));
    assert!(app.reviews.is_none());
    assert!(status(&mut app).contains("fetching PR #9…"));
    assert_eq!(
        said(&mut app, list, Said::Progress("late".to_owned())),
        None
    );
    let nine = Session {
        id: "9".to_owned(),
        ..session(&dir, HEAD, None)
    };
    let (revisions, comments, _) = range(said(&mut app, prepare, Said::Session(nine)));
    assert_eq!(revisions, format!("{BASE}...{HEAD}"));
    assert_eq!(comments, Some(dir.path().join("comments.json")));
    said(&mut app, prepare, Said::Progress("100/312".to_owned()));
    app.message = None;
    assert!(
        status(&mut app).contains("PR #9 · fetching threads 100/312"),
        "{}",
        status(&mut app)
    );
    said(
        &mut app,
        prepare,
        Said::Done(Err("threads incomplete".to_owned())),
    );
    let shown = status(&mut app);
    assert!(shown.contains("PR #9: threads incomplete"), "{shown}");
    app.message = None;
    assert!(!status(&mut app).contains("fetching"));
}

/// What the plugin pushes about the review shown goes in the status
/// bar, new comments and a moved head; a push about another review, or
/// a head that's the session's, says nothing. `p` pulls the moved head
/// and shows the new range in place, through the same file of comments.
/// The review's words name the keys bound to what they point to: a key
/// bound elsewhere shows as that key, and a hint whose action has no
/// key is left out.
#[test]
fn hints_name_the_keys_bound() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    described(&mut app, DESCRIBED);
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(&mut app, prepare, Said::Done(Ok(())));
    let moved = Push::HeadMoved {
        id: "7".to_owned(),
        head: "cccc333".to_owned(),
    };
    app.event(Event::ReviewPush(moved));
    app.message = None;
    for bind in ["p=unbind", "u=review_pull", "m=unbind", "n=next_thread"] {
        app.keys.bind(bind).expect("valid");
    }
    let shown = status(&mut app);
    assert!(
        shown.contains("PR #7 updated · new commits · u to update"),
        "{shown}"
    );
    let hint = |hint| hint_words(&app.keys, hint);
    assert_eq!(
        hint(Hint::StepThreads).as_deref(),
        Some("n/M go between threads")
    );
    assert_eq!(hint(Hint::OtherBranches).as_deref(), Some("b lists them"));
    app.keys.bind("u=unbind").expect("valid");
    app.dirty = true;
    let shown = status(&mut app);
    assert!(
        shown.contains("PR #7 updated · new commits") && !shown.contains("to update"),
        "{shown}"
    );
}

/// Nothing asks the plugin on a timer.
#[test]
fn a_pushed_head_shows_and_p_updates_the_pr_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    described(&mut app, DESCRIBED);
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(&mut app, prepare, Said::Done(Ok(())));
    app.reviewing.mode.as_mut().expect("a PR").plans.due = None;
    let deadline = app.deadline().expect("idle, later");
    assert!(deadline + Duration::from_secs(5) > Instant::now() + session::IDLE_AFTER);
    assert_eq!(app.event(Event::Tick), None, "no request on a timer");

    let push = |app: &mut App, push| app.event(Event::ReviewPush(push));
    let id = || "7".to_owned();
    push(&mut app, Push::CommentsChanged { id: "9".to_owned() });
    push(
        &mut app,
        Push::HeadMoved {
            id: id(),
            head: HEAD.to_owned(),
        },
    );
    app.message = None;
    assert!(!status(&mut app).contains("p to update"));
    push(&mut app, Push::CommentsChanged { id: id() });
    let shown = status(&mut app);
    assert!(
        shown.contains("PR #7 · new comments · p to update"),
        "{shown}"
    );
    push(
        &mut app,
        Push::HeadMoved {
            id: id(),
            head: "cccc333".to_owned(),
        },
    );
    let shown = status(&mut app);
    assert!(
        shown.contains("PR #7 updated · new commits · p to update"),
        "{shown}"
    );
    push(&mut app, Push::Log("checking PR #7 failed".to_owned()));
    assert!(status(&mut app).contains("checking PR #7 failed"));

    let (pull, request) = plugin_run(press(&mut app, Key::Char('p')));
    assert_eq!(request, Request::Pull { id: id() });
    let (revisions, comments, moved) = range(said(
        &mut app,
        pull,
        Said::Session(session(&dir, "cccc333", None)),
    ));
    assert_eq!(revisions, format!("{BASE}...cccc333"));
    assert_eq!(comments, None, "the same comments");
    assert_eq!(moved, Some((HEAD.to_owned(), "cccc333".to_owned())));
    app.review_updated(Some(3));
    let shown = status(&mut app);
    assert!(shown.contains("PR #7 updated · 3 new commits"), "{shown}");
    said(&mut app, pull, Said::Done(Ok(())));
    app.message = None;
    assert!(!status(&mut app).contains("p to update"));

    // Up to date is said once the threads are in, not at the session
    // line, which comes before them.
    let (again, _) = plugin_run(press(&mut app, Key::Char('p')));
    assert_eq!(
        said(
            &mut app,
            again,
            Said::Session(session(&dir, "cccc333", None))
        ),
        None
    );
    assert!(!status(&mut app).contains("up to date"));
    said(&mut app, again, Said::Done(Ok(())));
    assert!(status(&mut app).contains("PR #7 is up to date"));
}

/// Without a key for a while on a review you're idle, which the plugin
/// is told once, waking nothing more; the next key ends it.
#[test]
fn idle_is_told_once_and_a_key_ends_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    described(&mut app, DESCRIBED);
    assert_eq!(app.deadline(), None, "not on a review");
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(&mut app, prepare, Said::Done(Ok(())));
    app.reviewing.mode.as_mut().expect("a PR").plans.due = None;
    app.last_input = (Instant::now().checked_sub(session::IDLE_AFTER))
        .and_then(|at| at.checked_sub(Duration::from_secs(1)))
        .expect("the clock is past it");
    app.message = None;
    assert_eq!(app.take_idle(), None);
    assert_eq!(app.event(Event::Tick), None);
    assert_eq!(app.take_idle(), Some(true));
    assert_eq!(app.deadline(), None, "nothing to wake for");
    app.event(Event::Tick);
    assert_eq!(app.take_idle(), None, "told once");
    key(&mut app, 'j');
    assert_eq!(app.take_idle(), Some(false));
    assert!(app.deadline().is_some());
}

/// `P` shows what goes, from the plan the dialog asks for, what waits
/// on the host, a warning when the PR moved, and the plugin's events,
/// starting on the first that submits; a letter picks one, `s` writes
/// the summary, `enter` posts. The status bar follows the post, and `o`
/// opens the review it made. The summary is kept in the comments file
/// until a submitting event sends it.
#[test]
fn the_post_dialog_sends_the_event_and_summary_picked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    assert_eq!(path, dir.path().join("comments.json"), "the session's");
    app.reviewing.mode.as_mut().expect("a PR").moved = Some("cccc333".to_owned());
    let (plan, request) = plugin_run(press(&mut app, Key::Char('P')));
    assert_eq!(request, dry_run(None));
    let shown = screen(&mut app);
    assert!(shown.contains("post · PR #7"), "{shown}");
    assert!(
        shown.contains("5 comments") && shown.contains("checking what can be posted…"),
        "{shown}"
    );
    said(
        &mut app,
        plan,
        Said::Planned {
            line: 5,
            file: 0,
            skip: 0,
            pending: 2,
            resolves: 1,
        },
    );
    said(&mut app, plan, Said::Done(Ok(())));
    let shown = screen(&mut app);
    assert!(
        shown.contains("5 comments · 1 resolve change · 2 pending on PR #7"),
        "{shown}"
    );
    assert!(!shown.contains("checking"), "{shown}");
    assert!(shown.contains("PR #7 has new commits"), "{shown}");
    assert!(
        shown.contains("(•) c  comment") && shown.contains("( ) r  request changes"),
        "the plugin's events, the first that submits picked: {shown}"
    );
    key(&mut app, 'a');
    key(&mut app, 's');
    typed(&mut app, "Ship it");
    press(&mut app, Key::Esc);
    assert!(app.posting.is_some(), "esc leaves the summary first");
    let (post, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, self::post("approve", Some("Ship it")));
    assert_eq!(read_comments(&path).summary.as_deref(), Some("Ship it"));
    said(&mut app, post, Said::Progress("3/6".to_owned()));
    app.message = None;
    assert!(status(&mut app).contains("PR #7 · posting 3/6"));
    assert_eq!(press(&mut app, Key::Char('p')), None, "no update meanwhile");
    let url = "https://github.com/acme/widgets/pull/7#pullrequestreview-1";
    said(
        &mut app,
        post,
        Said::Posted {
            posted: 5,
            failed: 0,
            pending: 2,
            url: Some(url.to_owned()),
            last_reviewed_head: Some(HEAD.to_owned()),
            labels: None,
        },
    );
    said(&mut app, post, Said::Done(Ok(())));
    let shown = status(&mut app);
    assert!(shown.contains(&format!("review sent · {url}")), "{shown}");
    assert_eq!(
        press(&mut app, Key::Char('o')),
        Some(Action::Browse(url.to_owned()))
    );
    let mode = app.reviewing.mode.as_ref().expect("a PR");
    assert_eq!(mode.session.last_reviewed_head.as_deref(), Some(HEAD));
    assert_eq!(read_comments(&path).summary, None, "sent");
    // The next opening starts on the first event that submits again;
    // a plan that fails says so where the counts' check would.
    app.reviewing.job = None;
    let (plan, ..) = plugin_run(press(&mut app, Key::Char('P')));
    let shown = screen(&mut app);
    assert!(shown.contains("(•) c  comment"), "{shown}");
    said(&mut app, plan, Said::Done(Err("no session".to_owned())));
    let shown = screen(&mut app);
    assert!(
        shown.contains("couldn't check what can be posted: no session"),
        "{shown}"
    );
}

/// A plugin whose events all leave the review open starts the dialog on
/// the one last sent.
#[test]
fn without_a_submitting_event_the_dialog_starts_on_the_last_sent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 30);
    described(
        &mut app,
        r#"{"version":1,"command":"cr","noun":"change","nouns":"changes","events":[{"id":"draft","key":"d","label":"keep as draft","done":"kept","submits":false},{"id":"share","key":"h","label":"share","done":"shared","submits":false}]}"#,
    );
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    press(&mut app, Key::Char('P'));
    assert!(screen(&mut app).contains("(•) d  keep as draft"));
    key(&mut app, 'h');
    let (post, ..) = plugin_run(press(&mut app, Key::Enter));
    said(&mut app, post, Said::Done(Ok(())));
    press(&mut app, Key::Char('P'));
    assert!(screen(&mut app).contains("(•) h  share"));
}

/// A post that changed your votes says the labels as they are now: the
/// next dialog starts on them, and a vote left there sends no label.
#[test]
fn the_votes_a_post_left_start_the_next_dialog() {
    use crate::plugin::review::{LabelValue, ReviewLabel};
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    let verified = |vote| {
        vec![ReviewLabel {
            name: "Verified".to_owned(),
            values: [(0, "No score"), (1, "Works")]
                .map(|(value, label)| LabelValue {
                    value,
                    label: label.to_owned(),
                })
                .to_vec(),
            vote,
        }]
    };
    let session = Session {
        labels: verified(0),
        ..session(&dir, HEAD, None)
    };
    app.start_review("test".to_owned(), session);
    app.reviewing.job = None;
    press(&mut app, Key::Char('P'));
    assert!(screen(&mut app).contains("Verified  ‹0›  No score"));
    let (post, _) = plugin_run(press(&mut app, Key::Enter));
    said(
        &mut app,
        post,
        Said::Posted {
            posted: 5,
            failed: 0,
            pending: 0,
            url: None,
            last_reviewed_head: None,
            labels: Some(verified(1)),
        },
    );
    said(&mut app, post, Said::Done(Ok(())));
    press(&mut app, Key::Char('P'));
    assert!(screen(&mut app).contains("Verified  ‹+1›  Works"));
    let (_, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, self::post("comment", None));
}

/// "Suggest change" left as it opened, the lines as they are and
/// nothing said, saves nothing.
#[test]
fn an_untouched_suggestion_saves_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = app(100, 30);
    app.set_comments(path.clone());
    screen(&mut app);
    key(&mut app, 'S');
    press(&mut app, Key::Esc);
    assert!(!path.exists() || read_comments(&path).comments.is_empty());
}

/// The summary outlasts the dialog, in the comments file, and comes
/// back with it; an event that leaves the review open keeps it.
#[test]
fn the_summary_is_kept_until_a_review_sends_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    press(&mut app, Key::Char('P'));
    key(&mut app, 's');
    typed(&mut app, "Two nits");
    press(&mut app, Key::Esc);
    press(&mut app, Key::Esc);
    assert!(app.posting.is_none());
    assert_eq!(read_comments(&path).summary.as_deref(), Some("Two nits"));
    press(&mut app, Key::Char('P'));
    assert!(screen(&mut app).contains("Two nits"));
    key(&mut app, 'p');
    let (post, ..) = plugin_run(press(&mut app, Key::Enter));
    said(
        &mut app,
        post,
        Said::Posted {
            posted: 5,
            failed: 0,
            pending: 0,
            url: None,
            last_reviewed_head: None,
            labels: None,
        },
    );
    said(&mut app, post, Said::Done(Ok(())));
    assert_eq!(read_comments(&path).summary.as_deref(), Some("Two nits"));
    let mode = app.reviewing.mode.as_ref().expect("a PR");
    assert_eq!(
        mode.session.last_reviewed_head, None,
        "the plugin records the head it reviewed, and recorded none"
    );
}

/// Comments in your pending review on the host show `pending`, and the
/// status bar says how many wait and that `P` submits them; with no
/// draft, an event that submits still goes.
#[test]
fn pending_comments_show_and_p_submits_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let json = r#"{"version":1,"comments":[
{"id":"p1","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":null,"new":4},"end":{"old":null,"new":4},"text":"Waiting","author":"","created":"2026-11-17T10:00:00Z","ref":"github:1","pending":true,"anchor":{"before":[],"lines":["line four"],"after":[]}}
]}"#;
    let mut app = reviewed(&dir, json, (160, 30));
    described(&mut app, DESCRIBED);
    app.reviewing.job = None;
    app.message = None;
    let shown = screen(&mut app);
    assert!(shown.contains("you · 2h ago · pending"), "{shown}");
    assert!(
        status(&mut app).contains("review 7 · 1 pending · P submits"),
        "{}",
        status(&mut app)
    );
    let (plan, ..) = plugin_run(press(&mut app, Key::Char('P')));
    said(
        &mut app,
        plan,
        Said::Planned {
            line: 0,
            file: 0,
            skip: 0,
            pending: 1,
            resolves: 0,
        },
    );
    said(&mut app, plan, Said::Done(Ok(())));
    assert!(screen(&mut app).contains("1 pending on review 7"));
    key(&mut app, 'a');
    let (_, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, post("approve", None));
}

/// The local review's file: a draft thread on new line 2 with a reply,
/// a draft whose line isn't in the PR's diff, and a posted comment.
const LOCAL: &str = r#"{"version":1,"comments":[
{"id":"l1","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":2,"new":2},"end":{"old":2,"new":2},"text":"Local one","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":["line 1"],"lines":["line 2"],"after":["line 3"]}},
{"id":"l2","path":"src/some/deeply/nested/module/file.rs","text":"Local reply","author":"","created":"2026-11-17T10:05:00Z","in_reply_to":"l1"},
{"id":"l9","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":null,"new":30},"end":{"old":null,"new":30},"text":"Not on this diff","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":[],"lines":["only in the working tree"],"after":[]}},
{"id":"lp","path":"src/some/deeply/nested/module/file.rs","text":"Posted from here","author":"","created":"2026-11-17T10:00:00Z","ref":"github:9"}
]}"#;

/// The PR's file: your draft on new line 4, and a posted comment on
/// new line 50.
const PR_FILE: &str = r#"{"version":1,"comments":[
{"id":"pd","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":null,"new":4},"end":{"old":null,"new":4},"text":"PR draft","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":[],"lines":["line four"],"after":[]}},
{"id":"pp","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":null,"new":50},"end":{"old":null,"new":50},"text":"Fifty?","author":"octocat","created":"2026-11-17T10:00:00Z","ref":"github:1","anchor":{"before":[],"lines":["line fifty"],"after":[]}}
]}"#;

/// A viewer on PR #7 with [`PR_FILE`], plain dffy's [`LOCAL`] beside it,
/// drawn once they're placed. Returns the PR's file and the local one.
fn pr_with_local(dir: &tempfile::TempDir) -> (App, PathBuf, PathBuf) {
    let (pr, local) = (
        dir.path().join("comments.json"),
        dir.path().join("local.json"),
    );
    std::fs::write(&pr, PR_FILE).expect("write");
    std::fs::write(&local, LOCAL).expect("write");
    let mut app = app(100, 30);
    described(&mut app, DESCRIBED);
    app.set_local_comments(Some(local.clone()));
    app.start_review("test".to_owned(), session(dir, HEAD, None));
    app.reviewing.job = None;
    app.set_comments(pr.clone());
    screen(&mut app);
    (app, pr, local)
}

/// A local draft written elsewhere comes into view once it's placed,
/// and the drafts are planned again then.
#[test]
fn a_local_draft_placed_later_is_planned_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _, local) = pr_with_local(&dir);
    let due = |app: &App| app.reviewing.mode.as_ref().and_then(|m| m.plans.due);
    app.reviewing.mode.as_mut().expect("a PR").plans.due = None;
    let draft = r#"{"id":"l7","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":null,"new":2},"end":{"old":null,"new":2},"text":"Written elsewhere","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":[],"lines":["line 3"],"after":[]}}"#;
    let drafts = LOCAL.strip_suffix("\n]}").expect("a list");
    std::fs::write(&local, format!("{drafts},\n{draft}\n]}}")).expect("write");
    app.event(Event::Comments);
    assert_eq!(due(&app), None, "not in view yet");
    assert!(screen(&mut app).contains("Written elsewhere"));
    assert!(due(&app).is_some());
}

/// On a PR, the local review's drafts that place on its diff show with
/// a `local` chip and count as drafts; one that doesn't place stays out
/// of view and of the `i` list, and a posted one isn't brought. Edits
/// and replies in a local thread go to the local file, a new comment to
/// the PR's.
#[test]
fn local_drafts_show_on_a_pr_and_are_written_where_they_live() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, pr, local) = pr_with_local(&dir);
    let shown = screen(&mut app);
    assert!(shown.contains("draft · local"), "{shown}");
    assert!(shown.contains("Local reply"), "{shown}");
    assert!(!shown.contains("Not on this diff") && !shown.contains("Posted from here"));
    assert_eq!(
        app.sending(true),
        session::Sending {
            comments: 3,
            local: 2,
            ..session::Sending::default()
        }
    );
    key(&mut app, 'i');
    let list = screen(&mut app);
    assert!(
        list.contains("Local one") && !list.contains("Not on this diff"),
        "{list}"
    );
    press(&mut app, Key::Esc);

    let pr_before = std::fs::read(&pr).expect("read");
    app.follow_thread("l1");
    // `e` takes the thread's last draft: the reply.
    key(&mut app, 'e');
    typed(&mut app, " more");
    press(&mut app, Key::Esc);
    key(&mut app, 'r');
    typed(&mut app, "Again");
    press(&mut app, Key::Esc);
    let replies: Vec<_> = (read_comments(&local).comments.into_iter())
        .filter(|c| c.in_reply_to.as_deref() == Some("l1"))
        .map(|c| c.text)
        .collect();
    assert_eq!(replies, ["Local reply more", "Again"]);
    assert_eq!(std::fs::read(&pr).expect("read"), pr_before, "not the PR's");
    press(&mut app, Key::Esc);
    key(&mut app, 'c');
    typed(&mut app, "New here");
    press(&mut app, Key::Esc);
    let texts: Vec<String> = read_comments(&pr)
        .comments
        .into_iter()
        .map(|c| c.text)
        .collect();
    assert_eq!(texts, ["PR draft", "Fifty?", "New here"]);
    assert_eq!(read_comments(&local).comments.len(), 5);
}

/// The post dialog plans the local drafts in view with the review's,
/// through `--drafts`, and sends them only with `l`: those written
/// before the session started aren't sent unasked. Sending moves them
/// into the PR's file, with new ids, before `post` runs, and out of the
/// local file after; when the PR's file can't take them, nothing is
/// posted, the dialog stays and both files are as they were.
#[test]
fn posting_moves_the_local_drafts_into_the_prs_file_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, pr, local) = pr_with_local(&dir);
    let (check, request) = plugin_run(press(&mut app, Key::Char('P')));
    assert_eq!(request, dry_run(Some((&local, &["l1"]))), "the one in view");
    said(&mut app, check, draft("l1", Plan::File));
    said(
        &mut app,
        check,
        Said::Planned {
            line: 2,
            file: 1,
            skip: 0,
            pending: 0,
            resolves: 0,
        },
    );
    said(&mut app, check, Said::Done(Ok(())));
    let shown = screen(&mut app);
    assert!(
        shown.contains("1 comment") && shown.contains("l  [ ] with the 2 local drafts"),
        "written before the session: {shown}"
    );
    key(&mut app, 'l');
    let shown = screen(&mut app);
    assert!(
        shown.contains("3 comments · 1 as a file comment")
            && shown.contains("l  [x] with the 2 local drafts"),
        "{shown}"
    );

    std::fs::write(&pr, r#"{"version":2,"comments":[]}"#).expect("write");
    let local_before = std::fs::read(&local).expect("read");
    assert_eq!(press(&mut app, Key::Enter), None);
    assert!(app.posting.is_some(), "the dialog stays");
    assert!(status(&mut app).contains("nothing posted"));
    assert_eq!(std::fs::read(&local).expect("read"), local_before);
    assert_eq!(
        std::fs::read_to_string(&pr).expect("read"),
        r#"{"version":2,"comments":[]}"#
    );

    std::fs::write(&pr, PR_FILE).expect("write");
    let (_, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, post("comment", None));
    let left: Vec<String> = read_comments(&local)
        .comments
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(left, ["l9", "lp"]);
    let moved = read_comments(&pr).comments;
    let texts: Vec<&str> = moved.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(texts, ["PR draft", "Fifty?", "Local one", "Local reply"]);
    assert!(moved[2].id != "l1" && moved[2].reference.is_none());
    assert_eq!(moved[3].in_reply_to.as_ref(), Some(&moved[2].id));
    let shown = screen(&mut app);
    assert!(
        shown.contains("Local one") && !shown.contains("· local"),
        "{shown}"
    );
}

fn draft(id: &str, plan: Plan) -> Said {
    Said::Draft(Draft {
        id: id.to_owned(),
        path: FILE.to_owned(),
        plan,
    })
}

fn skip(why: &str) -> Plan {
    Plan::Skip(why.to_owned())
}

fn focused(app: &App) -> Option<&str> {
    app.doc.focused().map(|t| t.root.as_str())
}

/// What the heading of [`FILE`]'s comment `id` says posting it does.
fn posting(app: &App, id: &str) -> Option<(Role, String)> {
    let threads = app.comments.as_ref()?.threads(FILE.as_bytes())?;
    let note = threads
        .iter()
        .flat_map(|t| &t.comments)
        .find(|n| n.id == id)?;
    let posting = note.posting.clone()?;
    let role = if posting.warn { Role::Warn } else { Role::Dim };
    Some((role, posting.words))
}

/// The dialog asks for a plan beside the run going on and counts by
/// it, with one warning for the drafts that won't go: `v` closes the
/// dialog on the first, its thread focused, the summary kept; `m` goes
/// on through the threads from there. Each draft's heading says what
/// posting it does.
#[test]
fn the_post_dialog_warns_of_drafts_that_wont_go_and_v_goes_there() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    screen(&mut app);
    let (check, request) = plugin_run(press(&mut app, Key::Char('P')));
    assert_eq!(request, dry_run(None));
    assert!(
        app.reviewing.job.as_ref().is_some_and(|r| r.id == prepare),
        "the prepare goes on"
    );
    said(&mut app, check, draft("d1", Plan::Line));
    said(
        &mut app,
        check,
        draft("d2", skip("its thread isn't posted")),
    );
    said(&mut app, check, draft("f1", skip("")));
    said(&mut app, check, draft("m1", Plan::File));
    said(&mut app, check, Said::Done(Ok(())));
    let shown = screen(&mut app);
    assert!(
        shown.contains("5 comments · 1 as a file comment"),
        "{shown}"
    );
    assert!(
        shown.contains("! 2 drafts left out · v goes to them"),
        "{shown}"
    );
    key(&mut app, 's');
    typed(&mut app, "Later");
    press(&mut app, Key::Esc);

    key(&mut app, 'v');
    assert!(app.posting.is_none());
    assert_eq!(read_comments(&path).summary.as_deref(), Some("Later"));
    assert_eq!(focused(&app), Some("d1"), "the reply's thread");
    let shown = status(&mut app);
    assert!(
        shown.contains("not posted: its thread isn't posted · 1 more · m/M go between threads"),
        "{shown}"
    );
    let shown = screen(&mut app);
    assert!(
        shown.contains("Because.") && shown.contains("draft · not in the review yet"),
        "{shown}"
    );
    assert_eq!(posting(&app, "d1"), None, "posts on its line");
    assert_eq!(
        posting(&app, "d2"),
        Some((Role::Warn, "its thread isn't posted".to_owned())),
        "the plugin's reason"
    );
    assert_eq!(
        posting(&app, "f1"),
        Some((Role::Warn, "not in the review yet".to_owned())),
        "no reason given"
    );
}

/// A draft the host refused before says why in its heading, and the
/// dialog warns of it; a post that leaves drafts says how many were
/// refused, and the review is sent without them.
#[test]
fn refused_drafts_say_why_and_the_post_says_how_it_went() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let refuse = |path: &PathBuf, why: &str| {
        review::apply(
            path,
            vec![Op::Set {
                target: Target::Id("d1".to_owned()),
                change: Change {
                    post_error: Some(Some(why.to_owned())),
                    ..Change::default()
                },
            }],
        )
        .expect("applies");
    };
    refuse(&path, "Line could not be resolved");
    app.event(Event::Comments);
    let shown = screen(&mut app);
    assert!(shown.contains("draft · refused: Line could"), "{shown}");
    let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
    said(&mut app, check, Said::Done(Ok(())));
    assert!(screen(&mut app).contains("! 1 refused before · v goes to them"));
    key(&mut app, 'a');
    let (post, ..) = plugin_run(press(&mut app, Key::Enter));
    said(
        &mut app,
        post,
        draft("d1", skip("Line could not be resolved")),
    );
    said(&mut app, post, draft("g1", skip("file not in the PR")));
    said(
        &mut app,
        post,
        Said::Posted {
            posted: 3,
            failed: 2,
            pending: 0,
            url: None,
            last_reviewed_head: None,
            labels: None,
        },
    );
    refuse(&path, "Line could not be resolved");
    said(&mut app, post, Said::Done(Ok(())));
    let shown = status(&mut app);
    assert!(
        shown.contains("review sent · 1 comment refused, kept as drafts · 1 draft left out"),
        "{shown}"
    );
}

/// An event that leaves the review open on the host says it in its own
/// words; with nothing posted, it says so.
#[test]
fn an_event_that_leaves_the_review_open_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    for (posted, failed, said_after) in [
        (2, 1, "review pending · 1 draft left out"),
        (0, 0, "nothing posted"),
    ] {
        if let Some((check, ..)) = press(&mut app, Key::Char('P')).map(|a| plugin_run(Some(a))) {
            said(&mut app, check, Said::Done(Ok(())));
        }
        key(&mut app, 'p');
        let (post, request) = plugin_run(press(&mut app, Key::Enter));
        assert_eq!(request, self::post("pending", None));
        if failed > 0 {
            said(&mut app, post, draft("g1", skip("file not in the PR")));
        }
        said(
            &mut app,
            post,
            Said::Posted {
                posted,
                failed,
                pending: 0,
                url: None,
                last_reviewed_head: None,
                labels: None,
            },
        );
        said(&mut app, post, Said::Done(Ok(())));
        let shown = status(&mut app);
        assert!(shown.contains(said_after), "{shown}");
    }
}

/// A plugin that names no events posts with one, `post`.
#[test]
fn without_events_a_review_is_posted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 30);
    described(
        &mut app,
        r#"{"version":1,"command":"cr","noun":"change","nouns":"changes"}"#,
    );
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
    said(&mut app, check, Said::Done(Ok(())));
    let shown = screen(&mut app);
    assert!(shown.contains("(•) p  post"), "{shown}");
    let (post, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, self::post("post", None));
    said(
        &mut app,
        post,
        Said::Posted {
            posted: 5,
            failed: 0,
            pending: 0,
            url: None,
            last_reviewed_head: None,
            labels: None,
        },
    );
    said(&mut app, post, Said::Done(Ok(())));
    assert!(status(&mut app).contains("change 7 · review sent"));
}

/// While a post runs, the first quit says how far it got and that
/// another stops it; the second quits. After the post, one quit does.
#[test]
fn a_quit_while_posting_asks_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
    said(&mut app, check, Said::Done(Ok(())));
    let (post, ..) = plugin_run(press(&mut app, Key::Enter));
    said(&mut app, post, Said::Progress("37/200".to_owned()));
    assert_eq!(press(&mut app, Key::Char('q')), None);
    assert!(status(&mut app).contains("still posting 37/200 · q again stops it"));
    assert_eq!(press(&mut app, Key::Char('q')), Some(Action::Quit));
    said(&mut app, post, Said::Done(Ok(())));
    assert_eq!(press(&mut app, Key::Char('q')), Some(Action::Quit));
}

/// The plugin takes requests side by side, so a post waits for the
/// update running: the dialog stays, its summary with it, and posts
/// once the update is done.
#[test]
fn a_post_waits_for_the_update_running() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let (pull, request) = plugin_run(press(&mut app, Key::Char('p')));
    assert_eq!(request, Request::Pull { id: "7".to_owned() });
    if let Some((check, ..)) = press(&mut app, Key::Char('P')).map(|a| plugin_run(Some(a))) {
        said(&mut app, check, Said::Done(Ok(())));
    }
    key(&mut app, 's');
    typed(&mut app, "Nits");
    press(&mut app, Key::Esc);
    key(&mut app, 'c');
    assert_eq!(press(&mut app, Key::Enter), None, "nothing asked");
    let shown = status(&mut app);
    assert!(
        shown.contains("still updating; post once it's done"),
        "{shown}"
    );
    assert!(app.posting.is_some(), "the dialog stays");
    assert_eq!(read_comments(&path).summary, None, "nothing written");
    said(&mut app, pull, Said::Done(Ok(())));
    let (_, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, post("comment", Some("Nits")));
}

/// While a post runs, the plugin writes what it posted to the file: a
/// draft edited then would go up as it was, or not be found. The text
/// stays in its box, saved once the post is done, and an update after
/// keeps it.
#[test]
fn a_draft_edited_while_posting_waits_in_its_box() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
    said(&mut app, check, Said::Done(Ok(())));
    let (post, ..) = plugin_run(press(&mut app, Key::Enter));
    app.follow_thread("d1");
    key(&mut app, 'e');
    typed(&mut app, " Mostly.");
    press(&mut app, Key::Esc);
    assert!(app.doc.writing().is_some(), "still open");
    assert!(status(&mut app).contains("posting; edit once it's done"));
    press(&mut app, Key::Esc);
    assert!(
        app.doc.writing().is_some(),
        "a second esc doesn't discard it"
    );
    let text = |path: &Path| {
        (read_comments(path).comments.into_iter())
            .find(|c| c.id == "d2")
            .map(|c| c.text)
    };
    assert_eq!(text(&path).as_deref(), Some("Because."));
    said(&mut app, post, Said::Done(Ok(())));
    press(&mut app, Key::Esc);
    assert!(app.doc.writing().is_none());
    assert_eq!(text(&path).as_deref(), Some("Because. Mostly."));
    let (pull, ..) = plugin_run(press(&mut app, Key::Char('p')));
    said(&mut app, pull, Said::Done(Ok(())));
    assert_eq!(text(&path).as_deref(), Some("Because. Mostly."));
}

/// A signal or the terminal going away keeps the post dialog's summary
/// for the next time, as closing the dialog does.
#[test]
fn stopping_keeps_the_post_dialogs_summary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    press(&mut app, Key::Char('P'));
    key(&mut app, 's');
    typed(&mut app, "Half written");
    app.interrupted();
    assert_eq!(
        read_comments(&path).summary.as_deref(),
        Some("Half written")
    );
}

/// A thread of yours deleted on the host, kept for your draft reply in
/// it, isn't posted again as a new thread unasked: its heading says the
/// host deleted it, and the post dialog warns of it.
#[test]
fn a_thread_of_yours_deleted_on_the_host_is_flagged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let text = |s: &str| plugin::json::Value::String(s.to_owned());
    let mine = Op::Upsert {
        fields: vec![
            ("ref".to_owned(), text("github:1")),
            ("author".to_owned(), text("")),
        ],
        reply_to: None,
    };
    review::apply(&path, vec![mine]).expect("yours");
    let reply = Op::Add {
        comment: Box::new(review::Comment {
            text: "Still?".to_owned(),
            ..review::Comment::default()
        }),
        reply_to: Some(Target::Id("p1".to_owned())),
    };
    review::apply(&path, vec![reply]).expect("replies");
    app.event(Event::Comments);
    let before = app.sending(true);
    review::apply(&path, vec![Op::Gone(vec!["github:1".to_owned()])]).expect("gone");
    app.event(Event::Comments);
    let sending = app.sending(true);
    assert_eq!(
        (sending.comments, sending.refused),
        (before.comments + 1, before.refused + 1)
    );
    let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
    said(&mut app, check, Said::Done(Ok(())));
    let shown = screen(&mut app);
    assert!(shown.contains("refused before · v goes to them"), "{shown}");
    assert_eq!(
        posting(&app, "p1"),
        Some((Role::Warn, "refused: deleted on the host".to_owned()))
    );
}

/// The drafts are planned in the background a moment after they
/// change, never per frame, and the plan's words go in their
/// headings; a plan that fails says nothing.
#[test]
fn drafts_are_planned_in_the_background_after_they_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let due = app.deadline().expect("a plan is due");
    assert!(due <= Instant::now() + Duration::from_secs(1), "soon");
    app.reviewing.mode.as_mut().expect("a PR").plans.due = Some(Instant::now());
    let (plan, request) = plugin_run(app.event(Event::Tick));
    assert_eq!(request, dry_run(None));
    assert_eq!(app.event(Event::Tick), None, "one at a time");
    said(&mut app, plan, draft("d1", Plan::File));
    said(&mut app, plan, Said::Done(Ok(())));
    let file_comment = Some((Role::Dim, "posts as file comment".to_owned()));
    assert_eq!(posting(&app, "d1"), file_comment);
    assert_eq!(app.event(Event::Tick), None, "nothing changed since");
    app.follow_thread("d1");

    key(&mut app, 'r');
    typed(&mut app, "More");
    press(&mut app, Key::Esc);
    app.reviewing.mode.as_mut().expect("a PR").plans.due = Some(Instant::now());
    let (plan, ..) = plugin_run(app.event(Event::Tick));
    app.message = None;
    said(&mut app, plan, Said::Done(Err("nope".to_owned())));
    assert!(!status(&mut app).contains("nope"), "{}", status(&mut app));
    assert_eq!(posting(&app, "d1"), file_comment, "the last plan stays");
}

/// The plugin's notice, written as its post ends, is what the status
/// bar says after it, over dffy's own words for how it went, which the
/// review's part keeps; whether the watcher read it first or the
/// run's end did.
#[test]
fn the_plugins_notice_outlasts_the_post() {
    let url = "https://example.com/review/7";
    for watched in [false, true] {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        app.resize(160, 30);
        described(&mut app, DESCRIBED);
        app.start_review("test".to_owned(), session(&dir, HEAD, None));
        app.reviewing.job = None;
        let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
        said(&mut app, check, Said::Done(Ok(())));
        key(&mut app, 'c');
        let (post, ..) = plugin_run(press(&mut app, Key::Enter));
        said(
            &mut app,
            post,
            Said::Posted {
                posted: 3,
                failed: 0,
                pending: 0,
                url: Some(url.to_owned()),
                last_reviewed_head: None,
                labels: None,
            },
        );
        review::apply(
            &path,
            vec![Op::Notice(Some("a resolve was refused".to_owned()))],
        )
        .expect("applies");
        if watched {
            app.event(Event::Comments);
        }
        said(&mut app, post, Said::Done(Ok(())));
        let shown = status(&mut app);
        assert!(
            shown.contains("a resolve was refused"),
            "{watched}: {shown}"
        );
        assert!(shown.contains("PR #7 · review sent"), "{watched}: {shown}");
    }
}

/// "Since last review" switches to the range from the head last
/// reviewed and back; without one it says so.
#[test]
fn since_last_review_toggles_the_range() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    key(&mut app, ' ');
    assert_eq!(press(&mut app, Key::Char('R')), None);
    assert!(status(&mut app).contains("no review of yours posted"));
    app.reviewing
        .mode
        .as_mut()
        .expect("a PR")
        .session
        .last_reviewed_head = Some("9999aaa".to_owned());
    key(&mut app, ' ');
    let (revisions, ..) = range(press(&mut app, Key::Char('R')));
    assert_eq!(revisions, format!("9999aaa..{HEAD}"));
    assert!(status(&mut app).contains("since your last review: 9999aaa..bbbb222"));
    assert_eq!(app.description, "PR #7 since your review");
    key(&mut app, ' ');
    let (revisions, ..) = range(press(&mut app, Key::Char('R')));
    assert_eq!(revisions, format!("{BASE}...{HEAD}"));
}

/// The branch's PR, found by `detect`, is prepared and brings its
/// comments to plain dffy's range, which stays; "Show whole pull
/// request" shows its base against the working tree, and goes back.
/// With no PR, nothing changes.
#[test]
fn a_detected_pr_keeps_the_range_and_adds_its_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    described(&mut app, DESCRIBED);
    let plain = Revisions {
        unstaged: true,
        ..Revisions::default()
    };
    app.set_selected(plain.clone(), DefaultDiff::Unstaged);
    let title = app.description.clone();
    let (id, request) = plugin_run(Some(app.detect_review("test".to_owned())));
    assert_eq!(request, Request::Detect);
    let (prepare, request) = plugin_run(said(
        &mut app,
        id,
        Said::Detected(Some(session(&dir, HEAD, None))),
    ));
    assert_eq!(request, self::prepare("7"));
    said(&mut app, id, Said::Done(Ok(())));
    match said(&mut app, prepare, Said::Session(session(&dir, HEAD, None))) {
        Some(Action::Retarget {
            revisions,
            comments,
            ..
        }) => {
            assert_eq!(revisions, None, "the range stays");
            assert_eq!(comments, Some(dir.path().join("comments.json")));
        }
        other => panic!("not a retarget: {other:?}"),
    }
    assert_eq!(app.description, title);
    let whole = |app: &mut App| {
        key(app, ' ');
        match press(app, Key::Char('Q')) {
            Some(Action::Retarget {
                revisions: Some(revisions),
                comments: None,
                ..
            }) => revisions,
            other => panic!("not a retarget: {other:?}"),
        }
    };
    let revisions = whole(&mut app);
    assert_eq!(revisions.revs, [BASE]);
    assert!(revisions.merge_base && !revisions.staged);
    assert_eq!(app.description, "PR #7 + working tree");
    assert_eq!(app.reviewing.whole_title(), "Show working tree changes");
    assert_eq!(whole(&mut app), plain);
    assert_eq!(app.description, title);
    assert_eq!(app.reviewing.whole_title(), "Show whole pull request");

    let mut app = self::app(160, 20);
    let (id, _) = plugin_run(Some(app.detect_review("test".to_owned())));
    assert_eq!(said(&mut app, id, Said::Detected(None)), None);
    said(&mut app, id, Said::Done(Ok(())));
    assert!(app.reviewing.mode.is_none() && app.reviewing.job.is_none());
}

/// Listing PRs runs beside a `prepare`, which keeps saying how far it
/// got.
#[test]
fn listing_prs_leaves_the_run_going_on() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    described(&mut app, DESCRIBED);
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    key(&mut app, ' ');
    let (list, request) = plugin_run(press(&mut app, Key::Char('O')));
    assert_eq!(request, Request::List);
    said(&mut app, prepare, Said::Progress("2/9".to_owned()));
    said(&mut app, list, Said::Done(Ok(())));
    app.message = None;
    let shown = status(&mut app);
    assert!(shown.contains("PR #7 · fetching threads 2/9"), "{shown}");
}

#[test]
fn review_keys_refuse_a_piped_diff_and_need_a_pr() {
    let mut app = app(100, 20);
    assert_eq!(press(&mut app, Key::Char('p')), None);
    assert!(screen(&mut app).contains("not on a review"));
    app.set_piped();
    app.reviewing.plugin = Some("test".to_owned());
    key(&mut app, ' ');
    assert_eq!(press(&mut app, Key::Char('O')), None);
    assert!(screen(&mut app).contains("a piped diff can't be reviewed"));
}

/// Threads show under the lines they're on, each comment with its
/// author, age and state, replies indented; the comment on the whole
/// file under its first row. The gutter keeps a mark per thread, the
/// tree counts each file's comments, and the status bar counts the
/// threads, and in a review session the drafts.
#[test]
fn threads_show_inline_under_their_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    insta::assert_snapshot!("comment_marks", screen(&mut app));
    app.resize(160, 30);
    let wide = screen(&mut app);
    assert!(wide.contains("6 threads ─"), "{wide}");
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(&mut app, prepare, Said::Done(Ok(())));
    let wide = screen(&mut app);
    assert!(wide.contains("6 threads · 5 drafts"), "{wide}");
}

/// Files with comments beyond the ones on screen and prefetched are
/// diffed so their threads get placed; until then the comments panel
/// says it's still looking, and it lists the far file's threads once
/// they are, those on a file not in the diff included.
#[test]
fn far_files_with_comments_are_diffed_and_every_thread_listed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    screen(&mut app);
    let far = 11;
    assert_eq!(app.doc.document.files()[far].change().path, "src/zz9.rs");
    assert!(
        app.jobs()
            .iter()
            .any(|j| matches!(j, Job::Diff { file, .. } if *file == far)),
        "the far file is diffed"
    );
    key(&mut app, 'i');
    let listed = left(&mut app);
    assert!(listed.contains("finding the lines"), "{listed}");
    assert!(!listed.contains("Moved"), "{listed}");
    let loaded = loaded(diff("src/zz9.rs"));
    app.event(Event::Diffed(Done::Diff {
        placed: None,
        file: far,
        version: app.version(far),
        result: Ok(Box::new(loaded)),
    }));
    assert!(
        !app.jobs()
            .iter()
            .any(|j| matches!(j, Job::Diff { file, .. } if *file == far)),
        "placed, it isn't diffed again"
    );
    let listed = left(&mut app);
    assert!(
        listed.contains("Moved") && listed.contains("Gone"),
        "{listed}"
    );
}

/// [`commented`] with `json` for its comments, in a review session so
/// your comments are drafts, every file placed, `width` × `height`.
fn reviewed(dir: &tempfile::TempDir, json: &str, (width, height): (usize, usize)) -> App {
    let (mut app, path) = commented(dir);
    std::fs::write(&path, json).expect("write");
    let prepare = app.start_review("test".to_owned(), session(dir, HEAD, None));
    said(&mut app, prepare, Said::Done(Ok(())));
    app.resize(width, height);
    screen(&mut app);
    let far = diffed(&app, 11, diff("src/zz9.rs"));
    app.event(Event::Diffed(far));
    app.draw();
    place(&mut app);
    app
}

/// [`reviewed`] with the comments in the left panel.
fn listed(dir: &tempfile::TempDir, json: &str, size: (usize, usize)) -> App {
    let mut app = reviewed(dir, json, size);
    key(&mut app, 'i');
    app
}

/// The selected thread's root in the comments panel.
fn selected(app: &App) -> String {
    let panel = app.panel.as_ref().expect("the comments show");
    panel.selected().unwrap_or_default().to_owned()
}

/// The left panel's columns of the screen.
fn left(app: &mut App) -> String {
    let shown = screen(app);
    let width = app.tree_columns().unwrap_or(0);
    (shown.lines())
        .map(|l| l.chars().take(width).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The file shown and the root of the thread focused in it.
fn at(app: &App) -> (usize, String) {
    let focused = app.doc.focused().map(|t| t.root.clone());
    (app.doc.document.pos.file, focused.unwrap_or_default())
}

/// `json` with general comments, on the review rather than a file, as
/// a review plugin imports them: the description last in the file and
/// the others out of date order.
fn with_conversation(json: &str) -> String {
    let general = [
        r#"{"id":"c2","text":"Is this ready?","author":"hubot","created":"2026-11-16T09:00:00Z","ref":"h:2","kind":"comment"}"#,
        r#"{"id":"r1","text":"Looks good, one question inline.","author":"octocat","created":"2026-11-16T12:00:00Z","ref":"h:3","kind":"review","label":"approved","url":"https://example.com/7#review-1"}"#,
        r#"{"id":"c1","text":"Thanks for looking.","author":"","created":"2026-11-15T10:00:00Z","ref":"h:1","kind":"comment"}"#,
        r#"{"id":"d1","text":"Spells out five.\n\nThe count was off by one.","author":"ana","created":"2026-11-15T12:00:00Z","ref":"h:0","kind":"description"}"#,
    ];
    json.replacen(
        r#""comments":["#,
        &format!(r#""comments":[{},"#, general.join(",")),
        1,
    )
}

/// [`reviewed`] with general comments too.
fn conversing(dir: &tempfile::TempDir, (width, height): (usize, usize)) -> (App, PathBuf) {
    let (mut app, path) = commented(dir);
    std::fs::write(&path, with_conversation(&review_json("src/zz9.rs"))).expect("write");
    let prepare = app.start_review("test".to_owned(), session(dir, HEAD, None));
    said(&mut app, prepare, Said::Done(Ok(())));
    app.resize(width, height);
    screen(&mut app);
    // After the conversation, first.
    let far = 12;
    assert_eq!(app.doc.document.files()[far].change().path, "src/zz9.rs");
    app.event(Event::Diffed(Done::Diff {
        placed: None,
        file: far,
        version: app.version(far),
        result: Ok(Box::new(loaded(diff("src/zz9.rs")))),
    }));
    (app, path)
}

/// A review session with general comments has a Conversation first in
/// the tree, counted there and not as threads; the files keep their
/// place around it, and it goes with the last general comment. A local
/// review shows none: nothing there comes from a host.
#[test]
fn the_conversation_comes_first_in_a_review_with_general_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    std::fs::write(&path, with_conversation(&review_json("src/zz9.rs"))).expect("write");
    app.event(Event::Comments);
    app.resize(160, 30);
    let local = screen(&mut app);
    assert!(!app.doc.document.files()[0].is_page(), "{local}");
    assert!(
        !local.contains("Conversation") && local.contains("6 threads ─"),
        "{local}"
    );

    app.doc.goto_file(3);
    let on = app.doc.document.files()[3].change().path.clone();
    let built = model::conversed_count();
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(&mut app, prepare, Said::Done(Ok(())));
    let shown = screen(&mut app);
    assert!(app.doc.document.files()[0].is_page());
    let file = app.doc.document.pos.file;
    assert_eq!(
        (file, &app.doc.document.files()[file].change().path),
        (4, &on),
        "on its file still"
    );
    let first = shown.lines().nth(1).unwrap_or_default();
    assert!(
        first.contains("≡ Conversation") && first.contains(" 4 "),
        "{shown}"
    );
    assert!(shown.contains("6 threads · 5 drafts"), "{shown}");
    assert_eq!(
        model::conversed_count(),
        built,
        "nothing made until it shows"
    );

    std::fs::write(&path, review_json("src/zz9.rs")).expect("write");
    app.event(Event::Comments);
    let shown = screen(&mut app);
    assert!(!app.doc.document.files()[0].is_page(), "{shown}");
    let file = app.doc.document.pos.file;
    assert_eq!(
        (file, &app.doc.document.files()[file].change().path),
        (3, &on)
    );
}

/// The Conversation shows the review's title, then its description and
/// the other general comments by date, each in a thread's box with its
/// kind and label. Its comments are made once, however it scrolls, and
/// they're read-only.
#[test]
fn the_conversation_page_shows_the_title_then_each_comment_by_date() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = conversing(&dir, (72, 40));
    let written = std::fs::read(&path).expect("read");
    let built = model::conversed_count();
    app.open(0);
    insta::assert_snapshot!("conversation_page", screen(&mut app));
    assert_eq!(model::conversed_count(), built + 1);
    for _ in 0..6 {
        key(&mut app, 'j');
        screen(&mut app);
    }
    key(&mut app, 'g');
    screen(&mut app);
    assert_eq!(model::conversed_count(), built + 1, "made once");

    key(&mut app, 'm');
    assert_eq!(at(&app), (0, "d1".to_owned()));
    key(&mut app, 'r');
    assert!(status(&mut app).contains("read-only"));
    assert!(app.doc.writing().is_none());
    key(&mut app, ' ');
    assert_eq!(press(&mut app, Key::Char('E')), None, "nor in the editor");
    assert!(app.composing.is_none());
    assert_eq!(std::fs::read(&path).expect("read"), written);
}

/// `/` searches the Conversation's comments: counted from another file,
/// marked once it shows, and `enter` goes to them.
#[test]
fn the_search_finds_the_conversations_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = conversing(&dir, (72, 40));
    app.open(1);
    typed(&mut app, "/ready");
    let counted = app.jobs().into_iter().any(|j| {
        matches!(
            j,
            Job::Search {
                file: 0,
                change: None,
                ..
            }
        )
    });
    assert!(counted, "the page is counted by its comments alone");
    app.open(0);
    app.draw();
    let job = app.jobs().into_iter().find_map(|j| match j {
        Job::Find {
            file: 0,
            diff: None,
            hidden,
            blocks,
            blocks_version,
            needle,
            id,
            version,
            ..
        } => Some(Done::Found {
            file: 0,
            version,
            id,
            matches: document::find_blocks(&blocks, &needle, || false),
            diff: None,
            hidden,
            blocks_version,
        }),
        _ => None,
    });
    app.event(Event::Diffed(job.expect("a scan of the page")));
    let found = app
        .doc
        .document
        .search
        .as_ref()
        .map(|m| (m.file, m.matches.len()));
    assert_eq!(found, Some((0, 1)));
    press(&mut app, Key::Enter);
    assert!(
        status(&mut app).contains("match 1/"),
        "{}",
        status(&mut app)
    );
}

/// A file counted before its comments were placed is counted again
/// once they are: their text has matches too.
#[test]
fn a_file_is_counted_again_once_its_comments_are_placed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    typed(&mut app, "/Moved");
    let id = app.search.as_ref().expect("searching").id;
    let far = 11;
    app.event(Event::Diffed(Done::Search {
        file: far,
        version: app.version(far),
        id,
        count: Some(0),
    }));
    let asked = |app: &App| {
        (app.jobs().iter()).any(|j| matches!(j, Job::Search { file, .. } if *file == far))
    };
    assert!(!asked(&app), "counted");
    let placed = diffed(&app, far, diff("src/zz9.rs"));
    app.event(Event::Diffed(placed));
    assert!(asked(&app), "counted again, with its comments");
}

/// On the Conversation, with no code and no gutter, a drag in a comment
/// copies its Markdown as on a file's thread, across its paragraphs.
#[test]
fn a_drag_on_the_conversation_copies_a_comment() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = conversing(&dir, (72, 40));
    app.open(0);
    let (x, y) = cell_of(&mut app, "five.");
    let (tx, ty) = cell_of(&mut app, "count");
    app.event(Event::Input(Input::Click { x, y }));
    app.event(Event::Input(Input::Drag { x: tx + 4, y: ty }));
    let dragged = Some(copied("five.\n\nThe count", "copied 16 chars of a comment"));
    assert_eq!(
        app.event(Event::Input(Input::Release { x: tx + 4, y: ty })),
        dragged
    );
    // A click there, then a shift-click, selects the same.
    app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
    app.clicks = None;
    app.event(Event::Input(Input::Click { x, y }));
    app.event(Event::Input(Input::Release { x, y }));
    app.event(Event::Input(Input::ShiftClick { x: tx + 4, y: ty }));
    assert_eq!(
        app.event(Event::Input(Input::Release { x: tx + 4, y: ty })),
        dragged
    );
}

/// The comments panel lists the general comments first, apart from the
/// files' threads, which alone its title counts; picking one shows it
/// on the Conversation.
#[test]
fn the_comments_panel_lists_the_conversation_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = conversing(&dir, (130, 30));
    key(&mut app, 'i');
    let listed = left(&mut app);
    let lines: Vec<&str> = listed.lines().collect();
    assert!(
        lines[0].starts_with("┌ Comments · 6 threads · 5 open ─"),
        "{listed}"
    );
    assert!(
        lines[1].contains("Conversation") && lines[1].trim_end().ends_with("4 │"),
        "{listed}"
    );
    let order: Vec<usize> = ["Spells out", "Thanks for", "Is this ready", "Loo"]
        .iter()
        .map(|t| lines.iter().position(|l| l.contains(t)).expect(t))
        .collect();
    assert!(order.is_sorted(), "{listed}");
    assert!(lines[order[3]].contains("review approved"), "{listed}");
    assert!(lines[order[3] + 1].contains(FILE.rsplit('/').next().expect("a name")));
    key(&mut app, 'g');
    assert_eq!(at(&app), (0, "d1".to_owned()));
    key(&mut app, 'f');
    let open = left(&mut app);
    assert!(
        !open.contains("Conversation"),
        "the filters keep threads: {open}"
    );
}

/// `i` turns the left panel to the comment threads, showing it when
/// it's hidden, with the keys, and back to the files; its title says
/// which. Nothing of the list is made while the files show, however
/// the comments change.
#[test]
fn i_switches_the_left_panel_between_files_and_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let json = review_json("src/zz9.rs");
    let mut app = reviewed(&dir, &json, (100, 20));
    key(&mut app, 't');
    assert!(!app.tree_shown);
    let made = review::listing::listed_count();
    key(&mut app, 'i');
    assert!(app.tree_shown && app.pane == Pane::Tree);
    let shown = screen(&mut app);
    assert!(shown.starts_with("┌ Comments · 6 threads ─"), "{shown}");
    assert_eq!(review::listing::listed_count(), made + 1);
    assert!(
        app.take_count_pass().is_none(),
        "no counting for a tree not shown"
    );
    key(&mut app, 'i');
    assert!(app.panel.is_none() && app.pane == Pane::Tree);
    assert!(app.take_count_pass().is_some());
    let shown = screen(&mut app);
    assert!(shown.starts_with("┌ Files · 12 "), "{shown}");
    let path = dir.path().join("comments.json");
    std::fs::write(&path, json.replace("Why two?", "Why 2?")).expect("write");
    app.event(Event::Comments);
    assert!(screen(&mut app).contains("Why 2?"));
    assert_eq!(review::listing::listed_count(), made + 1);
}

/// On a narrow screen the comments open over the diff; `enter` closes
/// them on the thread, and `i` brings them back with the keys.
#[test]
fn narrow_comments_open_over_the_diff_and_come_back_with_i() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = reviewed(&dir, &review_json("src/zz9.rs"), (40, 30));
    key(&mut app, 'i');
    assert!(app.tree_shown && app.pane == Pane::Tree);
    key(&mut app, 'j');
    press(&mut app, Key::Enter);
    assert!(!app.tree_shown);
    assert_eq!((app.pane, at(&app)), (Pane::Diff, (0, "d1".to_owned())));
    key(&mut app, 'i');
    assert!(app.tree_shown && app.pane == Pane::Tree);
    let shown = screen(&mut app);
    assert!(
        shown
            .lines()
            .nth(1)
            .is_some_and(|l| l.starts_with("┌ Comments")),
        "{shown}"
    );
}

/// A thread listed before its file's diff is in, one on the whole
/// file: following it opens the file and waits, as `m` does, then
/// focuses it once the file's threads are placed.
#[test]
fn following_a_thread_whose_file_isnt_diffed_yet_waits_for_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    let json = review_json("src/zz9.rs");
    let whole = r#"{"id":"z1","path":"src/zz9.rs","text":"On the far file","created":"2026-11-17T09:00:00Z"}"#;
    let json = format!("{},{whole}]}}", json.strip_suffix("]}").expect("a list"));
    std::fs::write(&path, json).expect("write");
    app.event(Event::Comments);
    screen(&mut app);
    key(&mut app, 'i');
    for _ in 0..4 {
        key(&mut app, 'j');
    }
    assert_eq!(selected(&app), "z1");
    assert_eq!(at(&app), (11, String::new()));
    assert!(status(&mut app).contains("waiting for this file's diff"));
    let far = diffed(&app, 11, diff("src/zz9.rs"));
    app.event(Event::Diffed(far));
    assert_eq!(at(&app), (11, "z1".to_owned()));
}

/// The comments beside the diff: a header per file (its folder dimmed,
/// its name bold, its threads), then a row per thread: its mark, its
/// line, the author when the row above in the file has another, the
/// first line, and its replies at the right (`↩1`). A thread whose lines
/// weren't found says `moved`, unless its file isn't in the diff. A
/// narrow panel leaves out the line, then the replies.
#[test]
fn the_comments_panel_lists_one_quiet_row_per_thread() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = reviewed(&dir, &review_json("src/zz9.rs"), (130, 22));
    app.tree_width = Some(46);
    app.relayout();
    key(&mut app, 'i');
    insta::assert_snapshot!("comment_list", screen(&mut app));
    let narrow: Vec<String> = [26, 24, 22, 20]
        .into_iter()
        .map(|width| {
            app.tree_width = Some(width);
            app.relayout();
            left(&mut app)
                .lines()
                .take(11)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();
    insta::assert_snapshot!("comment_list_narrow", narrow.join("\n\n"));
}

/// A thread's row has its mark in its look's role, its line in the
/// line numbers', `moved` dimmed in the warning's, and its author bold;
/// a resolved one is dimmed whole, its author still bold. File names
/// are bold.
#[test]
fn comment_rows_take_their_roles() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = reviewed(&dir, &review_json("src/zz9.rs"), (130, 22));
    app.tree_width = Some(46);
    app.relayout();
    key(&mut app, 'i');
    // Distinct styles for the roles the marks and rows take.
    app.theme = Theme::parse(
        "roles",
        "warn = yellow underline\nhint = cyan underline\ndim = dim underline\n\
             line-number = blue underline\n",
        &mut Vec::new(),
    );
    app.invalidate();
    let ink = |role: Role, attributes: Attributes, label: &str| {
        let ink = Ink {
            role,
            syntax: None,
            attributes,
        };
        (app.theme.ink_style(ink), label.to_owned())
    };
    let styles = [
        ink(Role::Warn, Attributes::NONE, "warn"),
        ink(Role::Hint, Attributes::NONE, "hint"),
        ink(Role::Dim, Attributes::NONE, "dim"),
        ink(Role::LineNumber, Attributes::NONE, "line-number"),
        ink(Role::Warn, Attributes::DIM, "warn+dim"),
        ink(Role::Dim, Attributes::BOLD, "dim+bold"),
        ink(Role::Plain, Attributes::BOLD, "bold"),
    ];
    let markup = style_markup(&mut app, &styles, 46);
    insta::assert_snapshot!(
        "comment_list_roles",
        markup
            .lines()
            .skip(1)
            .take(9)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Moving through the comments shows each thread in the diff, focused,
/// loading its file when it's another, while the keys stay in the
/// panel; opening it moves nothing. A thread on a file not in the diff
/// says so and leaves nothing focused. The wheel and a tap move the
/// selection too.
#[test]
fn moving_through_the_comments_shows_each_thread_in_the_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
    assert_eq!(selected(&app), "f1");
    assert_eq!(at(&app), (0, String::new()), "opening moves nothing");
    let mut seen = Vec::new();
    for _ in 0..5 {
        key(&mut app, 'j');
        seen.push(at(&app));
    }
    let at_ = |file: usize, root: &str| (file, root.to_owned());
    assert_eq!(
        seen,
        [
            at_(0, "d1"),
            at_(0, "p1"),
            at_(0, "r1"),
            at_(11, "m1"),
            at_(11, "")
        ]
    );
    assert!(status(&mut app).contains("r reply · e edit · d delete · x resolve"));
    assert_eq!(app.pane, Pane::Tree);
    key(&mut app, 'k');
    assert_eq!(at(&app), at_(11, "m1"));
    key(&mut app, 'g');
    assert_eq!(at(&app), at_(0, "f1"));
    app.event(Event::Input(Input::Scroll {
        delta: 1,
        x: 5,
        y: 5,
    }));
    assert_eq!((selected(&app), at(&app)), ("d1".to_owned(), at_(0, "d1")));
    // Under the header: f1, d1, p1, then r1.
    app.event(Event::Input(Input::Click { x: 5, y: 5 }));
    assert_eq!((selected(&app), at(&app)), ("r1".to_owned(), at_(0, "r1")));
    // The selection follows `m` in the diff.
    press(&mut app, Key::Tab);
    key(&mut app, 'm');
    assert_eq!(at(&app), at_(0, "p1"));
    screen(&mut app);
    assert_eq!(selected(&app), "p1");
}

/// Following a folded thread from the comments shows it whole until the
/// selection moves on. `z` on it in the diff keeps what it sets, a
/// reply keeps it whole, and closing the panel ends the peek.
#[test]
fn following_unfolds_a_folded_thread_while_its_selected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
    // f1, d1, then p1, outdated and posted, and r1, resolved.
    key(&mut app, 'j');
    key(&mut app, 'j');
    assert_eq!(selected(&app), "p1");
    assert!(!app.doc.fold().collapsed("p1"));
    key(&mut app, 'j');
    assert!(app.doc.fold().collapsed("p1") && !app.doc.fold().collapsed("r1"));
    press(&mut app, Key::Tab);
    key(&mut app, 'z');
    assert!(app.doc.fold().collapsed("r1"));
    press(&mut app, Key::Tab);
    key(&mut app, 'k');
    key(&mut app, 'j');
    assert_eq!(selected(&app), "r1");
    assert!(app.doc.fold().collapsed("r1"), "as z left it");
    key(&mut app, 'k');
    press(&mut app, Key::Tab);
    key(&mut app, 'r');
    assert!(app.doc.writing().is_some());
    press(&mut app, Key::Esc);
    press(&mut app, Key::Tab);
    key(&mut app, 'j');
    assert_eq!(selected(&app), "r1");
    assert!(!app.doc.fold().collapsed("p1"), "replied in");
    key(&mut app, 'i');
    assert!(app.panel.is_none());
    assert!(app.doc.fold().collapsed("r1"));
}

/// `enter` or `tab` hands the keys to the diff, the thread focused for
/// `r`, `x`, `e`, `d` and `o`; `tab` comes back to the comments, and
/// `esc` in them hands the keys to the diff as it does from the tree.
#[test]
fn enter_or_tab_hands_the_keys_to_the_diff_on_the_thread() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
    let path = dir.path().join("comments.json");
    key(&mut app, 'j');
    press(&mut app, Key::Enter);
    assert_eq!((app.pane, at(&app)), (Pane::Diff, (0, "d1".to_owned())));
    key(&mut app, 'x');
    assert!(
        read_comments(&path)
            .comments
            .iter()
            .any(|c| c.id == "d1" && c.resolved)
    );
    press(&mut app, Key::Tab);
    assert!(app.pane == Pane::Tree && app.panel.is_some());
    assert!(screen(&mut app).starts_with("┌ Comments"));
    key(&mut app, 'j');
    let root = selected(&app);
    press(&mut app, Key::Tab);
    assert_eq!((app.pane, at(&app).1), (Pane::Diff, root));
    press(&mut app, Key::Tab);
    press(&mut app, Key::Esc);
    assert!(app.pane == Pane::Diff && app.panel.is_some());
}

/// The thread's keys are its table's: rebound, the new key acts and
/// the status bar names it; unbound, the old one acts as it would
/// without a thread.
#[test]
fn thread_keys_follow_the_thread_table() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
    let path = dir.path().join("comments.json");
    let resolved = || (read_comments(&path).comments.iter()).any(|c| c.id == "g1" && c.resolved);
    for binding in ["thread:R=resolve", "thread:x=unbind"] {
        app.keys.bind(binding).expect("binds");
    }
    for _ in 0..5 {
        key(&mut app, 'j');
    }
    screen(&mut app);
    assert_eq!(
        note(&app).as_deref(),
        Some("r reply · e edit · d delete · R resolve")
    );
    key(&mut app, 'x');
    assert!(!resolved());
    key(&mut app, 'R');
    assert!(resolved());
}

/// On a thread whose file isn't in the diff, the panel's keys act on
/// the selected row: `x` resolves it, `r` and `e` write in the editor,
/// as the diff has no box to show, and `d` twice deletes the draft,
/// then the thread. The status bar lists them; `d` then a move keeps
/// the draft, and the search still takes every letter.
#[test]
fn the_comments_panel_acts_on_a_thread_not_in_the_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
    let path = dir.path().join("comments.json");
    let find = |id: &str| {
        read_comments(&path)
            .comments
            .into_iter()
            .find(|c| c.id == id)
    };
    for _ in 0..5 {
        key(&mut app, 'j');
    }
    assert_eq!(
        (selected(&app), at(&app).1),
        ("g1".to_owned(), String::new())
    );
    screen(&mut app);
    assert_eq!(
        note(&app).as_deref(),
        Some("r reply · e edit · d delete · x resolve")
    );
    app.resize(200, 30);
    let shown = status(&mut app);
    assert!(
        shown.contains("r reply") && shown.contains("f filter"),
        "{shown}"
    );
    key(&mut app, 'x');
    assert!(find("g1").is_some_and(|c| c.resolved));
    assert_eq!((app.pane, selected(&app)), (Pane::Tree, "g1".to_owned()));
    app.message = None;
    screen(&mut app);
    assert!(note(&app).is_some_and(|n| n.ends_with("x reopen")));
    key(&mut app, 'x');
    assert!(find("g1").is_some_and(|c| !c.resolved));

    assert_eq!(
        press(&mut app, Key::Char('r')),
        Some(Action::Compose(String::new()))
    );
    app.composed(Some("Noted".to_owned()));
    let reply = read_comments(&path)
        .comments
        .into_iter()
        .find(|c| c.text == "Noted")
        .expect("the reply");
    assert_eq!(reply.in_reply_to.as_deref(), Some("g1"));
    assert_eq!(
        press(&mut app, Key::Char('e')),
        Some(Action::Compose("Noted".to_owned()))
    );
    app.composed(Some("Noted!".to_owned()));
    assert_eq!(find(&reply.id).map(|c| c.text), Some("Noted!".to_owned()));

    key(&mut app, 'd');
    key(&mut app, 'k');
    key(&mut app, 'd');
    assert!(find("m1").is_some(), "a move disarms d");
    key(&mut app, 'j');
    key(&mut app, 'd');
    key(&mut app, 'd');
    assert!(find(&reply.id).is_none() && find("g1").is_some());
    key(&mut app, 'o');
    assert!(screen(&mut app).contains("this thread has no link"));
    key(&mut app, 'd');
    key(&mut app, 'd');
    assert!(find("g1").is_none());
    assert!(!left(&mut app).contains("Gone"));

    let before = read_comments(&path);
    app.message = None;
    key(&mut app, '/');
    assert_eq!(selected(&app), "m1");
    assert!(!note(&app).is_some_and(|n| n.contains("r reply")), "typing");
    typed(&mut app, "rexdo");
    assert!(app.panel.as_ref().is_some_and(|p| p.editing));
    assert!(left(&mut app).contains("no thread matches /rexdo"));
    assert_eq!(read_comments(&path), before, "the search takes them");
}

/// On a thread the diff shows, the panel's `r` and `e` open the box in
/// the diff, which takes the keys, and `x` acts without leaving the
/// panel.
#[test]
fn the_comments_panel_writes_in_the_diff_on_a_thread_shown_there() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
    let path = dir.path().join("comments.json");
    assert_eq!(selected(&app), "f1");
    assert_eq!(at(&app).1, "", "opening follows nothing");
    key(&mut app, 'j');
    assert_eq!(selected(&app), "d1");
    key(&mut app, 'x');
    assert!(
        read_comments(&path)
            .comments
            .iter()
            .any(|c| c.id == "d1" && c.resolved)
    );
    assert_eq!(app.pane, Pane::Tree);
    assert_eq!(press(&mut app, Key::Char('r')), None);
    let writes = |app: &App| app.doc.writing().map(|w| w.at.clone());
    assert_eq!(writes(&app), Some(Writes::Reply("d1".to_owned())));
    assert_eq!(app.pane, Pane::Diff);
    typed(&mut app, "Sure");
    press(&mut app, Key::Esc);
    assert!(
        read_comments(&path)
            .comments
            .iter()
            .any(|c| c.text == "Sure" && c.in_reply_to.as_deref() == Some("d1"))
    );
    // The thread the panel opens on isn't followed until it writes.
    key(&mut app, 'i');
    app.doc.set_focus(None);
    key(&mut app, 'i');
    assert_eq!(
        (selected(&app), at(&app).1),
        ("f1".to_owned(), String::new())
    );
    key(&mut app, 'e');
    assert!(matches!(writes(&app), Some(Writes::Edit { root, .. }) if root == "f1"));
}

/// A right click on a thread's row in the comments panel selects it and
/// lists its keys, without folding for one the diff doesn't show; each
/// entry acts on that thread.
#[test]
fn the_menu_of_a_panel_row_acts_on_its_thread() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
    let path = dir.path().join("comments.json");
    right_click(&mut app, "Gone");
    assert_eq!((app.pane, selected(&app)), (Pane::Tree, "g1".to_owned()));
    assert_eq!(
        menu_rows(&app),
        [
            "Reply r",
            "Edit draft e",
            "Delete draft d",
            "Resolve x",
            "Copy comment",
            "Copy thread",
        ]
    );
    assert!(choose(&mut app, "Resolve").is_none());
    assert!(
        read_comments(&path)
            .comments
            .iter()
            .any(|c| c.id == "g1" && c.resolved)
    );
    right_click(&mut app, "Gone");
    let copied = choose(&mut app, "Copy comment");
    assert!(matches!(&copied, Some(Action::Copy { text, .. }) if text == b"Gone"));
    right_click(&mut app, "Gone");
    assert_eq!(
        choose(&mut app, "Reply"),
        Some(Action::Compose(String::new()))
    );
    app.composed(None);
    right_click(&mut app, "Gone");
    choose(&mut app, "Delete draft");
    key(&mut app, 'd');
    assert!(!read_comments(&path).comments.iter().any(|c| c.id == "g1"));
}

/// `f` steps through All, Open, Drafts and Resolved, `F` back; the title
/// counts what's kept of every thread and names the filter. `/` keeps
/// the threads whose path, author or text has what's typed, with the
/// filter; `esc` clears it, then hands the keys to the diff. A filter
/// that keeps nothing says so.
#[test]
fn the_comments_panel_filters_by_state_and_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (130, 30));
    app.tree_width = Some(46);
    app.relayout();
    let title = |app: &mut App| screen(app).lines().next().unwrap_or_default().to_owned();
    assert!(title(&mut app).starts_with("┌ Comments · 6 threads · 5 open ─"));
    let texts = |app: &mut App| {
        let shown = left(app);
        [
            "On the whole",
            "Why two?",
            "Fifty?",
            "Old four",
            "Moved away",
            "Gone",
        ]
        .into_iter()
        .filter(|t| shown.contains(t))
        .collect::<Vec<_>>()
    };
    key(&mut app, 'f');
    assert_eq!(
        texts(&mut app),
        ["On the whole", "Why two?", "Fifty?", "Moved away", "Gone"]
    );
    assert!(title(&mut app).starts_with("┌ Comments · 5/6 · open ─"));
    key(&mut app, 'f');
    assert_eq!(
        texts(&mut app),
        ["On the whole", "Why two?", "Moved away", "Gone"]
    );
    key(&mut app, 'f');
    assert_eq!(texts(&mut app), ["Old four"]);
    assert_eq!(selected(&app), "r1");
    assert_eq!(at(&app).1, "r1", "the diff follows the selection");
    key(&mut app, 'F');
    key(&mut app, 'F');
    typed(&mut app, "/octo");
    assert_eq!(texts(&mut app), ["Fifty?"], "the author");
    assert!(screen(&mut app).contains("└« /octo▏ ─"));
    press(&mut app, Key::Esc);
    typed(&mut app, "/zz9");
    assert_eq!(texts(&mut app), ["Moved away"], "the path");
    let file = app.doc.document.pos.file;
    press(&mut app, Key::Down);
    app.event(Event::Input(Input::Key(Key::Char('n'), Mods::CTRL)));
    assert_eq!(
        app.doc.document.pos.file, file,
        "typing leaves the tree alone"
    );
    press(&mut app, Key::Enter);
    assert!(title(&mut app).starts_with("┌ Comments · 1/6 · open · /zz9 ─"));
    key(&mut app, 'f');
    key(&mut app, 'f');
    assert!(
        left(&mut app).contains("no thread matches"),
        "the filter keeps the search"
    );
    key(&mut app, 'F');
    key(&mut app, 'F');
    press(&mut app, Key::Esc);
    assert_eq!(app.pane, Pane::Tree, "esc clears the search first");
    typed(&mut app, "/because");
    assert_eq!(texts(&mut app), ["Why two?"], "a reply's text");
    press(&mut app, Key::Enter);
    press(&mut app, Key::Esc);
    for _ in 0..4 {
        key(&mut app, 'f');
    }
    assert_eq!(texts(&mut app).len(), 5, "round to Open");
    press(&mut app, Key::Esc);
    assert_eq!(app.pane, Pane::Diff);
}

/// A resolved thread with a draft reply in it is a draft: first among
/// its file's, and kept by Drafts as well as Resolved, since posting
/// sends it.
#[test]
fn a_resolved_thread_with_a_draft_in_it_lists_as_a_draft() {
    let dir = tempfile::tempdir().expect("tempdir");
    let json = review_json("src/zz9.rs");
    let reply = r#"{"id":"r2","path":"src/some/deeply/nested/module/file.rs","text":"Reopen?","author":"","created":"2026-11-17T11:00:00Z","in_reply_to":"r1"}"#;
    let json = format!("{},{reply}]}}", json.strip_suffix("]}").expect("a list"));
    let mut app = listed(&dir, &json, (100, 40));
    let mut seen = vec![selected(&app)];
    for _ in 0..3 {
        key(&mut app, 'j');
        seen.push(selected(&app));
    }
    assert_eq!(seen, ["f1", "d1", "r1", "p1"]);
    let listed = |app: &mut App| left(app).contains("Old four");
    key(&mut app, 'f');
    assert!(!listed(&mut app), "not open");
    key(&mut app, 'f');
    assert!(listed(&mut app), "a draft");
    key(&mut app, 'f');
    assert!(listed(&mut app), "resolved");
}

/// Comments read while the file's diff is being made go to the pool
/// before the next frame, and come placed with the diff: the frame that
/// shows the diff shows its threads.
#[test]
fn comments_read_while_a_diff_runs_come_with_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(diff(FILE).change),
        (100, 20),
        Indicators::Classic,
    );
    app.event(Event::EnumerationDone(None));
    app.draw();
    assert!(
        app.jobs()
            .iter()
            .any(|j| matches!(j, Job::Diff { file: 0, .. }))
    );
    let path = dir.path().join("comments.json");
    std::fs::write(&path, review_json("src/zz9.rs")).expect("write");
    app.set_comments(path);
    let annotations = app.take_annotations().expect("for the pool");
    assert!(app.take_annotations().is_none(), "taken once");
    let loaded = loaded(diff(FILE));
    app.event(Event::Diffed(Done::Diff {
        file: 0,
        version: app.version(0),
        placed: crate::tui::worker::place_on(&annotations, &loaded, false),
        result: Ok(Box::new(loaded)),
    }));
    app.draw();
    assert!(app.screen.text().contains("Why two?"));
}

/// A file off screen, diffed for its comments, has their lost code
/// parsed with the diff, which no frame waits for: dropped from memory
/// after, it isn't diffed again for it.
#[test]
fn a_far_file_is_painted_with_its_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    let far = 11;
    let asked = |app: &App| {
        (app.jobs().into_iter()).find_map(|j| match j {
            Job::Diff { file, paint, .. } if file == far => Some(paint),
            _ => None,
        })
    };
    app.draw();
    assert_eq!(asked(&app), Some(true));
    let loaded = loaded(diff("src/zz9.rs"));
    let annotations = app.comments.as_ref().map(Comments::annotations);
    let placed = annotations.and_then(|a| crate::tui::worker::place_on(a, &loaded, true));
    app.event(Event::Diffed(Done::Diff {
        file: far,
        version: app.version(far),
        placed,
        result: Ok(Box::new(loaded)),
    }));
    app.doc.document.evict(0, &app.visible);
    app.draw();
    assert_eq!(asked(&app), None, "placed and painted");
}

/// The diff of a file comes with its comments placed but the code of
/// those lost not parsed yet: a placement of its own paints it after,
/// once.
#[test]
fn lost_code_is_painted_after_the_diff_lands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(diff(FILE).change),
        (100, 20),
        Indicators::Classic,
    );
    app.event(Event::EnumerationDone(None));
    app.draw();
    let path = dir.path().join("comments.json");
    let lost = format!(
        r#"{{"version":1,"comments":[{{"id":"l1","path":"{FILE}","side":"new","start":{{"old":null,"new":9}},"end":{{"old":null,"new":9}},"text":"Gone","author":"","created":"2026-11-17T10:00:00Z","anchor":{{"before":[],"lines":["fn gone() -> u32 {{ 1 }}"],"after":[]}}}}]}}"#
    );
    std::fs::write(&path, lost).expect("write");
    app.set_comments(path);
    let annotations = app.take_annotations().expect("for the pool");
    let painted = |app: &App| {
        let threads = (app.comments.as_ref()).and_then(|c| c.threads(FILE.as_bytes()));
        let snippet = threads.and_then(|t| t.first()?.snippet.clone());
        let rows = snippet.expect("lost, with its code").rows;
        rows.iter()
            .flat_map(|r| &r.pieces)
            .any(|(s, _)| s.is_some())
    };
    let loaded = loaded(diff(FILE));
    app.event(Event::Diffed(Done::Diff {
        file: 0,
        version: app.version(0),
        placed: crate::tui::worker::place_on(&annotations, &loaded, false),
        result: Ok(Box::new(loaded)),
    }));
    assert!(!painted(&app), "not parsed with the diff");
    app.draw();
    assert!(place(&mut app), "a placement paints it");
    assert!(painted(&app));
    app.draw();
    assert!(!place(&mut app), "once");
}

/// A comment on lines written elsewhere is placed by the pool, asked for
/// right after the diffs on screen; a placement made before the
/// comments changed again, or on a diff since replaced, is dropped and
/// asked for again.
#[test]
fn placements_from_the_pool_land_unless_stale() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    let with = |extra: &[&str]| {
        let json = review_json("src/zz9.rs");
        let json = json.strip_suffix("]}").expect("a list");
        let added = extra.join(",");
        std::fs::write(&path, format!("{json},{added}]}}")).expect("write");
    };
    let comment = |id: &str, line: usize, text: &str| {
        format!(
            r#"{{"id":"{id}","path":"{FILE}","side":"new","start":{{"old":null,"new":{line}}},"end":{{"old":null,"new":{line}}},"text":"{text}","author":"","created":"2026-11-17T10:00:00Z","anchor":{{"before":[],"lines":["line {}"],"after":[]}}}}"#,
            line + 1
        )
    };
    let roots = |app: &App| -> Vec<String> {
        let threads = app
            .comments
            .as_ref()
            .and_then(|c| c.threads(FILE.as_bytes()));
        threads.map_or_else(Vec::new, |t| t.iter().map(|t| t.root.clone()).collect())
    };
    let place_job = |app: &mut App| {
        app.draw();
        let jobs = app.jobs();
        let at = jobs.iter().position(|j| matches!(j, Job::Place(_)));
        assert_eq!(at, Some(0), "first: the file shown is loaded");
        match jobs.into_iter().next() {
            Some(Job::Place(job)) => job,
            _ => unreachable!("checked above"),
        }
    };

    with(&[&comment("n1", 2, "New")]);
    app.event(Event::Comments);
    assert!(!roots(&app).contains(&"n1".to_owned()), "not placed yet");
    let stale = place_job(&mut app);
    with(&[&comment("n1", 2, "New"), &comment("n2", 5, "Newer")]);
    app.event(Event::Comments);
    app.event(Event::Diffed(stale.run()));
    assert!(!roots(&app).contains(&"n1".to_owned()), "placed before n2");

    let mut current = place_job(&mut app);
    let replaced = Arc::clone(&current.diff);
    current.diff = Arc::new(diff(FILE));
    app.event(Event::Diffed(current.run()));
    assert!(!roots(&app).contains(&"n1".to_owned()), "on another diff");
    let mut current = place_job(&mut app);
    assert!(Arc::ptr_eq(&current.diff, &replaced));
    current.diff = replaced;
    app.event(Event::Diffed(current.run()));
    let roots = roots(&app);
    assert!(roots.contains(&"n1".to_owned()) && roots.contains(&"n2".to_owned()));
}

/// `m` goes from thread to thread in the order the diff shows them,
/// focusing each, then on into the next file with comments, waiting for
/// its diff when it has none yet; `M` goes back across files too.
#[test]
fn m_steps_through_threads_across_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    screen(&mut app);
    let focused = |app: &App| {
        app.doc
            .focused()
            .map(|t| t.root.clone())
            .unwrap_or_default()
    };
    let mut seen = Vec::new();
    for _ in 0..4 {
        key(&mut app, 'm');
        seen.push(focused(&app));
        let shown = screen(&mut app);
        let first = app.doc.focused().map(|t| t.comments[0].text.clone());
        assert!(shown.contains(&first.unwrap_or_default()), "{shown}");
    }
    assert_eq!(seen, ["f1", "d1", "r1", "p1"]);
    key(&mut app, 'm');
    let far = 11;
    assert_eq!(app.doc.document.pos.file, far);
    assert!(screen(&mut app).contains("waiting for this file's diff"));
    let diffed = diffed(&app, far, diff("src/zz9.rs"));
    app.event(Event::Diffed(diffed));
    assert_eq!(focused(&app), "m1");
    key(&mut app, 'm');
    assert!(screen(&mut app).contains("no more threads below"));
    key(&mut app, 'M');
    assert_eq!(
        (app.doc.document.pos.file, focused(&app)),
        (0, "p1".to_owned())
    );
    key(&mut app, 'M');
    assert_eq!(focused(&app), "r1");
}

/// While a thread has the focus, `x` resolves and reopens it, `r` opens
/// a reply box in it, `e` edits your draft there, `d` twice deletes the
/// draft, and `esc` leaves. Without the focus the same keys keep their
/// own meaning: `r` the rendered view, `x` more lines.
#[test]
fn a_focused_thread_takes_its_keys_only_while_focused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    let find = |id: &str| {
        read_comments(&path)
            .comments
            .into_iter()
            .find(|c| c.id == id)
    };
    screen(&mut app);
    key(&mut app, 'r');
    assert!(screen(&mut app).contains("no renderer for .rs"));
    key(&mut app, 'x');
    assert!(app.doc.writing().is_none());
    assert!(find("d1").is_some_and(|c| !c.resolved));

    key(&mut app, 'm');
    key(&mut app, 'm');
    assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("d1"));
    app.message = None;
    let shown = screen(&mut app);
    assert!(
        shown.contains("r reply · e edit · d delete · x resolve"),
        "{shown}"
    );
    let hint = note(&app).unwrap_or_default();
    assert!(hint.ends_with("z fold · esc"), "{hint}");
    key(&mut app, 'z');
    let hint = note(&app).unwrap_or_default();
    assert!(hint.ends_with("z unfold · esc"), "{hint}");
    key(&mut app, 'z');
    key(&mut app, 'x');
    assert!(find("d1").is_some_and(|c| c.resolved));
    key(&mut app, 'x');
    assert!(find("d1").is_some_and(|c| !c.resolved));

    key(&mut app, 'r');
    typed(&mut app, "Sure");
    insta::assert_snapshot!("comment_reply", screen(&mut app));
    press(&mut app, Key::Esc);
    let reply = read_comments(&path)
        .comments
        .into_iter()
        .find(|c| c.text == "Sure")
        .expect("the reply");
    assert_eq!(reply.in_reply_to.as_deref(), Some("d1"));
    assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("d1"));

    key(&mut app, 'e');
    typed(&mut app, "!");
    press(&mut app, Key::Esc);
    assert_eq!(find(&reply.id).map(|c| c.text), Some("Sure!".to_owned()));
    key(&mut app, 'd');
    assert!(find(&reply.id).is_some(), "one d asks");
    key(&mut app, 'd');
    assert!(find(&reply.id).is_none() && find("d1").is_some());
    key(&mut app, 'o');
    assert!(screen(&mut app).contains("this thread has no link"));
    press(&mut app, Key::Esc);
    assert!(app.doc.focused().is_none());

    // Scrolled off screen, the thread lets go: `x` reveals lines again.
    assert!(app.doc.go_to_thread("d1"));
    press(&mut app, Key::PageDown);
    key(&mut app, 'x');
    assert!(app.doc.focused().is_none());
    assert!(find("d1").is_some_and(|c| !c.resolved));
}

/// A thread placed on lines a `⋯` separator hides reveals them, two
/// lines around them and no more: it hangs under its own line, between
/// a separator for the hidden lines above and one for those below, and
/// the view stays on the row it showed. `X` and back keeps them.
#[test]
fn a_thread_on_hidden_lines_reveals_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    std::fs::write(
            &path,
            r#"{"version":1,"comments":[{"id":"h1","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":20,"new":20},"end":{"old":20,"new":20},"text":"Hidden here","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":[],"lines":["line 20"],"after":[]}}]}"#,
        )
        .expect("write");
    let mut app = app(100, 12);
    // The `@@` row and the first hunk's eight, then the separator.
    app.doc.scroll(9);
    let top = |app: &mut App| screen(app).lines().nth(1).unwrap_or_default().to_owned();
    assert!(top(&mut app).contains("⋯ 39 unchanged lines"));
    let line = app.doc.line_at_top();
    app.set_comments(path);
    assert_eq!(app.doc.line_at_top(), line, "the view stays");
    let shown = screen(&mut app);
    assert!(top(&mut app).contains("⋯ 10 unchanged lines"), "{shown}");
    assert!(shown.contains("⋯ 24 unchanged lines"), "{shown}");
    let has_line = |n: usize| shown.contains(&format!("│ line {n}"));
    assert!(
        (18..=22).all(has_line) && !has_line(17) && !has_line(23),
        "{shown}"
    );
    let under = shown
        .lines()
        .skip_while(|l| !l.contains("│ line 20"))
        .nth(1)
        .unwrap_or_default();
    assert!(under.contains("▾ you · 2h ago ─"), "{shown}");
    key(&mut app, 'X');
    key(&mut app, 'X');
    assert!(screen(&mut app).contains("⋯ 10 unchanged lines"));
    key(&mut app, 'm');
    let shown = screen(&mut app);
    let under = shown
        .lines()
        .skip_while(|l| !l.contains("│ line 20"))
        .nth(1)
        .unwrap_or_default();
    assert!(under.contains("▾ you · 2h ago ━"), "{shown}");
}

/// `o` opens a posted thread on its host. Its comments are the host's:
/// `e` and `d` say so. Replying to an outdated thread says the review
/// plugin's warning once, when it has one.
#[test]
fn posted_threads_open_on_their_host_and_outdated_ones_warn() {
    let quiet = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&quiet);
    screen(&mut app);
    for _ in 0..4 {
        key(&mut app, 'm');
    }
    key(&mut app, 'r');
    assert!(!screen(&mut app).contains("outdated:"), "no warning to say");

    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    described(
        &mut app,
        r#"{"version":1,"command":"pr","noun":"pull request","nouns":"pull requests","outdated_reply_warning":"the host folds this thread, so a reply there is easy to miss"}"#,
    );
    screen(&mut app);
    for _ in 0..4 {
        key(&mut app, 'm');
    }
    assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("p1"));
    assert_eq!(
        press(&mut app, Key::Char('o')),
        Some(Action::Browse(
            "https://github.com/o/r/pull/1#discussion_r1".to_owned()
        ))
    );
    key(&mut app, 'e');
    assert!(screen(&mut app).contains("no draft here to edit"));
    key(&mut app, 'd');
    assert!(screen(&mut app).contains("no draft here to delete"));
    key(&mut app, 'r');
    assert!(screen(&mut app).contains("outdated: the host folds this thread"));
    press(&mut app, Key::Esc);
    key(&mut app, 'r');
    assert!(!screen(&mut app).contains("the host folds"), "once");
    press(&mut app, Key::Esc);
    assert_eq!(
        read_comments(&path).comments.len(),
        7,
        "empty replies discarded"
    );
}

/// The cell where `text` first shows on screen under the title row,
/// which names the file shown.
fn cell_of(app: &mut App, text: &str) -> (u16, u16) {
    let shown = screen(app);
    let (x, y) = shown
        .lines()
        .enumerate()
        .skip(1)
        .find_map(|(y, l)| Some((l[..l.find(text)?].chars().count(), y)))
        .unwrap_or_else(|| panic!("{text}:\n{shown}"));
    (
        u16::try_from(x).expect("column"),
        u16::try_from(y).expect("row"),
    )
}

fn right_click(app: &mut App, text: &str) {
    let (x, y) = cell_of(app, text);
    app.event(Event::Input(Input::RightClick { x, y }));
}

/// The open menu's entries, each its label and its key.
fn menu_rows(app: &App) -> Vec<String> {
    app.menu
        .as_ref()
        .expect("a menu")
        .entries()
        .iter()
        .map(|e| format!("{} {}", e.label, e.key).trim_end().to_owned())
        .collect()
}

/// Moves the menu's cursor to the entry starting `label`, `j` and
/// `down` alike, and runs it with `enter`.
fn choose(app: &mut App, label: &str) -> Option<Action> {
    let i = menu_rows(app)
        .iter()
        .position(|r| r.starts_with(label))
        .unwrap_or_else(|| panic!("{label}: {:?}", menu_rows(app)));
    for step in 0..i {
        if step % 2 == 0 {
            key(app, 'j');
        } else {
            press(app, Key::Down);
        }
    }
    let done = press(app, Key::Enter);
    assert!(app.menu.is_none(), "running an entry closes the menu");
    done
}

/// A right click on a file in the tree selects it as `j`/`k` would
/// and lists what the tree's keys do to it; each entry does what its
/// key does there.
#[test]
fn the_menu_of_a_tree_file_runs_the_trees_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    right_click(&mut app, "zz3.rs");
    let file = app.doc.document.pos.file;
    assert_eq!(app.doc.document.files()[file].change().path, "src/zz3.rs");
    assert_eq!(app.pane, Pane::Tree);
    assert_eq!(app.tree.item(), Some(Item::File(file)));
    assert_eq!(
        menu_rows(&app),
        [
            "Copy path Y",
            "Copy absolute path",
            "Open in editor e",
            "Mark viewed v",
            "Comment on file C",
            "Show only files with comments c",
        ]
    );
    let copied = choose(&mut app, "Copy path");
    assert_eq!(copied, press(&mut app, Key::Char('Y')));
    assert_eq!(
        choose_again(&mut app, "zz3.rs", "Copy absolute path"),
        Some(Action::CopyFullPath("src/zz3.rs".into()))
    );
    assert!(choose_again(&mut app, "zz3.rs", "Mark viewed").is_none());
    assert!(app.doc.document.files()[file].viewed());
    right_click(&mut app, "zz3.rs");
    assert!(menu_rows(&app).contains(&"Mark unviewed v".to_owned()));
    esc(&mut app);
    assert!(app.menu.is_none());
    key(&mut app, 'v');
    assert!(!app.doc.document.files()[file].viewed(), "the same as v");
    choose_again(&mut app, "zz3.rs", "Show only files");
    assert!(app.tree.commented());
    // A tap on an entry runs it; a tap outside closes the menu.
    right_click(&mut app, "zz9.rs");
    let (x, y) = cell_of(&mut app, "Show every file");
    app.event(Event::Input(Input::Click { x, y }));
    assert!(app.menu.is_none() && !app.tree.commented());
    right_click(&mut app, "zz3.rs");
    app.event(Event::Input(Input::Click { x: 99, y: 0 }));
    assert!(app.menu.is_none() && !app.tree.commented());
}

fn choose_again(app: &mut App, text: &str, label: &str) -> Option<Action> {
    right_click(app, text);
    choose(app, label)
}

/// On a line of code: a comment on it, its copy and the editor at it,
/// each the same as tapping the line and pressing the keys shown.
#[test]
fn the_menu_of_a_code_line_runs_its_keys_on_that_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    right_click(&mut app, "line 3");
    assert_eq!(app.pane, Pane::Diff);
    assert_eq!(
        menu_rows(&app),
        [
            "Comment on line c",
            "Suggest change S",
            "Copy line v y",
            "Open in editor e"
        ]
    );
    let copied = choose(&mut app, "Copy line");
    assert!(app.doc.selection.is_none());
    let tap = |app: &mut App, text: &str| {
        let (x, y) = cell_of(app, text);
        app.event(Event::Input(Input::Click { x, y }));
        app.event(Event::Input(Input::Release { x, y }));
    };
    tap(&mut app, "line 3");
    key(&mut app, 'v');
    assert_eq!(copied, press(&mut app, Key::Char('y')));
    assert!(matches!(&copied, Some(Action::Copy { text, .. }) if text == b"line 3"));

    let edit = choose_again(&mut app, "line 3", "Open in editor");
    assert_eq!(
        edit,
        Some(Action::Edit {
            path: FILE.into(),
            line: 3
        })
    );
    tap(&mut app, "line 3");
    assert_eq!(edit, press(&mut app, Key::Char('e')));

    choose_again(&mut app, "line 3", "Comment on line");
    let writes = |app: &App| app.doc.writing().map(|w| w.at.clone());
    let on_line = Some(Writes::New {
        side: document::Side::New,
        lines: 2..3,
    });
    assert_eq!(writes(&app), on_line);
    esc(&mut app);
    tap(&mut app, "line 3");
    key(&mut app, 'c');
    assert_eq!(writes(&app), on_line);
}

/// Inside the selection the entries act on it; outside, on the line
/// clicked, the selection gone.
#[test]
fn the_menu_inside_a_selection_acts_on_the_selection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    screen(&mut app);
    key(&mut app, 'v');
    key(&mut app, 'j');
    let selection = app.doc.selection.clone();
    right_click(&mut app, "line 2");
    assert_eq!(app.doc.selection, selection, "kept");
    assert_eq!(
        menu_rows(&app),
        [
            "Comment on selection c",
            "Suggest change S",
            "Copy selection y",
            "Open in editor e"
        ]
    );
    let copied = choose(&mut app, "Copy selection");
    assert!(matches!(&copied, Some(Action::Copy { text, .. }) if text == b"line 1\nline 2"));
    assert!(app.doc.selection.is_none(), "as y ends it");
    // From the line right-clicked, as after a tap.
    key(&mut app, 'v');
    key(&mut app, 'j');
    right_click(&mut app, "line 5");
    assert!(app.doc.selection.is_none());
    assert_eq!(menu_rows(&app)[0], "Comment on line c");
}

/// On a `⋯` separator, `x` for that one, though `x` alone reveals the
/// first on screen: the same as tapping it.
#[test]
fn the_menu_of_a_separator_reveals_its_gap() {
    let tapped = |menu: bool| {
        let mut app = app(100, 40);
        let shown = screen(&mut app);
        let lines: Vec<&str> = shown.lines().collect();
        let separators: Vec<usize> = (0..lines.len())
            .filter(|&y| lines[y].contains('⋯'))
            .collect();
        let [.., _, y] = separators[..] else {
            panic!("two separators:\n{shown}");
        };
        let x = lines[y].chars().position(|c| c == '⋯').expect("⋯");
        let (x, y) = (
            u16::try_from(x).expect("column"),
            u16::try_from(y).expect("row"),
        );
        if menu {
            app.event(Event::Input(Input::RightClick { x, y }));
            assert_eq!(menu_rows(&app), ["Reveal hidden lines x"]);
            choose(&mut app, "Reveal");
        } else {
            app.event(Event::Input(Input::Click { x, y }));
        }
        screen(&mut app)
    };
    assert_eq!(tapped(true), tapped(false));
}

/// On a thread: what its keys do once it has the focus, as far as they
/// apply. Edit and delete only with a draft in it, opening on the host
/// only with a link.
#[test]
fn the_menu_of_a_thread_runs_its_focused_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    right_click(&mut app, "Why two?");
    assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("d1"));
    assert_eq!(
        menu_rows(&app),
        [
            "Reply r",
            "Edit draft e",
            "Delete draft d",
            "Resolve x",
            "Fold z",
            "Copy comment",
            "Copy thread",
        ]
    );
    choose(&mut app, "Edit draft");
    assert_eq!(
        app.doc.writing().map(|w| w.text.text().to_owned()),
        Some("Because.".to_owned())
    );
    esc(&mut app);
    choose_again(&mut app, "Why two?", "Delete draft");
    assert!(status(&mut app).contains("d again deletes your draft reply"));
    key(&mut app, 'd');
    assert_eq!(read_comments(&path).comments.len(), 6);
    choose_again(&mut app, "Why two?", "Fold");
    assert!(app.doc.fold().collapsed("d1"));
    choose_again(&mut app, "Why two?", "Unfold");
    assert!(!app.doc.fold().collapsed("d1"));

    esc(&mut app);
    key(&mut app, 'g');
    for _ in 0..4 {
        key(&mut app, 'm');
    }
    esc(&mut app);
    assert!(app.doc.focused().is_none());
    right_click(&mut app, "Fifty?");
    assert_eq!(
        menu_rows(&app),
        [
            "Reply r",
            "Resolve x",
            "Unfold z",
            "Open on host o",
            "Copy comment",
            "Copy thread"
        ]
    );
    let opened = choose(&mut app, "Open on host");
    assert_eq!(opened, press(&mut app, Key::Char('o')));
    assert!(matches!(opened, Some(Action::Browse(_))));
    choose_again(&mut app, "Fifty?", "Resolve");
    assert!(
        read_comments(&path)
            .comments
            .iter()
            .any(|c| c.id == "p1" && c.resolved)
    );
}

/// A viewer of [`app`]'s file with a thread on its new line 2: ana's
/// comment in Markdown, then your reply.
fn marked(dir: &tempfile::TempDir, config: &Config) -> App {
    let mut app = app_of(config, "working tree", diff(FILE), (100, 30));
    let path = dir.path().join("comments.json");
    let json = format!(
        r#"{{"version":1,"comments":[{{"id":"d1","path":"{FILE}","side":"new","start":{{"old":null,"new":2}},"end":{{"old":null,"new":2}},"text":"Use **two blockers** at `03f1ba`.\n\n- [x] tests pass","author":"ana","created":"2026-11-17T10:00:00Z","anchor":{{"before":[],"lines":["line 2"],"after":[]}}}},{{"id":"d2","path":"{FILE}","text":"Because.","author":"","created":"2026-11-17T11:00:00Z","in_reply_to":"d1"}}]}}"#
    );
    std::fs::write(&path, json).expect("write");
    app.set_comments(path);
    app
}

const MARKED: &str = "Use **two blockers** at `03f1ba`.\n\n- [x] tests pass";

fn copied(text: &str, note: &str) -> Action {
    Action::Copy {
        text: text.as_bytes().to_vec(),
        note: note.to_owned(),
    }
}

/// A drag in a comment selects its text by characters and copies the
/// Markdown it was drawn from when let go. Past the comment it stops at
/// the comment's end, not in the reply below. Neither the drag nor the
/// frames it draws render the comment again.
#[test]
fn a_drag_in_a_comment_copies_its_markdown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = marked(&dir, &Config::default());
    let (x, y) = cell_of(&mut app, "two");
    let to = cell_of(&mut app, "blockers").0 + 7;
    let rendered = crate::tui::markdown::rendered_count();
    app.event(Event::Input(Input::Click { x, y }));
    for x in x + 1..=to {
        app.event(Event::Input(Input::Drag { x, y }));
        app.draw();
    }
    let done = app.event(Event::Input(Input::Release { x: to, y }));
    app.draw();
    assert_eq!(
        crate::tui::markdown::rendered_count(),
        rendered,
        "no comment rendered again"
    );
    assert_eq!(
        done,
        Some(copied("**two blockers**", "copied 16 chars of a comment"))
    );
    assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("d1"));

    let (rx, ry) = cell_of(&mut app, "Because.");
    app.event(Event::Input(Input::Click { x, y }));
    app.event(Event::Input(Input::Drag { x: rx, y: ry }));
    assert_eq!(
        app.event(Event::Input(Input::Release { x: rx, y: ry })),
        Some(copied(
            "**two blockers** at `03f1ba`.\n\n- [x] tests pass",
            "copied 47 chars of a comment"
        ))
    );
}

/// A click in a comment focuses its thread and copies nothing; a
/// double click copies the word, a triple click the whole comment as
/// written. A click on the thread's top line still folds it.
#[test]
fn clicks_in_a_comment_copy_a_word_or_the_comment() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = marked(&dir, &Config::default());
    let (x, y) = cell_of(&mut app, "blockers");
    let click = |app: &mut App| {
        app.event(Event::Input(Input::Click { x, y }));
        app.event(Event::Input(Input::Release { x, y }))
    };
    assert_eq!(click(&mut app), None);
    assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("d1"));
    assert!(!app.doc.fold().collapsed("d1"));
    let word = Some(copied("blockers", "copied 8 chars of a comment"));
    assert_eq!(click(&mut app), word);
    assert_eq!(press(&mut app, Key::Char('y')), word, "y copies it again");
    assert_eq!(click(&mut app), Some(copied(MARKED, "copied a comment")));
    key(&mut app, 'j');
    assert!(app.doc.comment_selection.is_none(), "another key ends it");
    let (x, y) = cell_of(&mut app, "▾ ana");
    app.event(Event::Input(Input::Click { x, y }));
    app.event(Event::Input(Input::Release { x, y }));
    assert!(app.doc.fold().collapsed("d1"));
}

/// A drag that starts on code selects code, even when it goes on into a
/// comment.
#[test]
fn a_drag_from_code_into_a_comment_copies_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = marked(&dir, &Config::default());
    let (x, y) = cell_of(&mut app, "line 2");
    let (cx, cy) = cell_of(&mut app, "two");
    app.event(Event::Input(Input::Click { x, y }));
    app.event(Event::Input(Input::Drag { x: x + 5, y }));
    app.event(Event::Input(Input::Drag { x: cx, y: cy }));
    assert_eq!(
        app.event(Event::Input(Input::Release { x: cx, y: cy })),
        Some(copied("line 2", "copied 6 chars"))
    );
}

/// On a thread the menu also copies the comment right-clicked as it was
/// written, and the whole thread, each comment under its author and
/// date. They have no keys unless bound, and bound, show them.
#[test]
fn the_menu_of_a_thread_copies_a_comment_or_the_thread() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::default();
    config.apply_source("keybind = alt+c=copy_thread\n", &mut Vec::new());
    let mut app = marked(&dir, &config);
    right_click(&mut app, "Because.");
    assert_eq!(
        menu_rows(&app)[menu_rows(&app).len() - 2..],
        ["Copy comment", "Copy thread alt+c"]
    );
    assert_eq!(
        choose(&mut app, "Copy comment"),
        Some(copied("Because.", "copied a comment"))
    );
    assert_eq!(
        choose_again(&mut app, "two", "Copy comment"),
        Some(copied(MARKED, "copied a comment"))
    );
    let thread = format!("ana · 2026-11-17\n{MARKED}\n\nyou · 2026-11-17\nBecause.");
    let whole = Some(copied(&thread, "copied a thread of 2 comments"));
    assert_eq!(choose_again(&mut app, "Because.", "Copy thread"), whole);
    assert_eq!(
        app.event(Event::Input(Input::Key(Key::Char('c'), Mods::ALT))),
        whole
    );
}

/// `C`, or the tree menu's entry, writes a comment on the whole file in
/// a box under its first row.
#[test]
fn c_on_a_file_comments_on_the_whole_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = app(100, 20);
    app.set_comments(path.clone());
    choose_again(&mut app, "file.rs", "Comment on file");
    assert_eq!(app.pane, Pane::Diff, "the box is in the diff");
    typed(&mut app, "Whole");
    let shown = screen(&mut app);
    let at = |text: &str| {
        shown
            .find(text)
            .unwrap_or_else(|| panic!("{text}:\n{shown}"))
    };
    assert!(at("Whole▏") < at("line 1"), "{shown}");
    esc(&mut app);
    let saved = read_comments(&path);
    let [comment] = saved.comments.as_slice() else {
        panic!("one comment: {saved:?}");
    };
    assert_eq!(
        (comment.path.as_str(), comment.text.as_str()),
        (FILE, "Whole")
    );
    assert!(comment.start.is_none() && comment.anchor.is_none());
    assert_eq!(app.doc.focused().map(|t| &t.at), Some(&model::At::File));
    key(&mut app, 'C');
    assert_eq!(app.doc.writing().map(|w| &w.at), Some(&Writes::File));
    esc(&mut app);
    // A file not diffed yet takes one too: it needs no lines.
    choose_again(&mut app, "zz.rs", "Comment on file");
    typed(&mut app, "Later");
    assert!(screen(&mut app).contains("Later▏"));
    esc(&mut app);
    assert!(
        read_comments(&path)
            .comments
            .iter()
            .any(|c| c.path == "src/zz.rs")
    );
}

/// The menu opens at the cell right-clicked, or as close as the screen
/// allows.
#[test]
fn the_menu_opens_at_the_click_within_the_screen() {
    let mut app = app(80, 14);
    screen(&mut app);
    app.event(Event::Input(Input::RightClick { x: 10, y: 4 }));
    let shown = screen(&mut app);
    assert_eq!(
        shown.lines().nth(4).and_then(|l| l.chars().nth(10)),
        Some('┌'),
        "{shown}"
    );
    esc(&mut app);
    app.event(Event::Input(Input::RightClick { x: 77, y: 12 }));
    insta::assert_snapshot!("menu_at_the_corner", screen(&mut app));
    // A right click elsewhere opens it there instead.
    app.event(Event::Input(Input::RightClick { x: 10, y: 4 }));
    let shown = screen(&mut app);
    assert_eq!(
        shown.lines().nth(4).and_then(|l| l.chars().nth(10)),
        Some('┌'),
        "{shown}"
    );
    assert_eq!(shown.matches('┌').count(), 1, "{shown}");
}

/// A closed menu costs nothing: the screen is as before it opened, and
/// the next frame reuses the diff's rows again.
#[test]
fn a_closed_menu_draws_nothing() {
    let mut app = app(80, 14);
    let before = screen(&mut app);
    app.event(Event::Input(Input::RightClick { x: 10, y: 4 }));
    assert_ne!(screen(&mut app), before);
    assert!(!app.kept, "an overlay over the rows");
    esc(&mut app);
    assert!(app.menu.is_none());
    assert_eq!(screen(&mut app), before);
    assert!(app.kept);
    // Nothing to act on, no menu: the status bar.
    app.event(Event::Input(Input::RightClick { x: 10, y: 13 }));
    assert!(app.menu.is_none());
}

/// `z` folds the focused thread to one row and back; `Z` folds all.
#[test]
fn z_folds_threads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    screen(&mut app);
    key(&mut app, 'm');
    key(&mut app, 'm');
    key(&mut app, 'z');
    let shown = screen(&mut app);
    assert!(
        shown.contains("▸ you · 2h ago · Why two?") && shown.contains("1 reply"),
        "{shown}"
    );
    assert!(!shown.contains("Because."));
    key(&mut app, 'z');
    assert!(screen(&mut app).contains("Because."));
    key(&mut app, 'Z');
    let shown = screen(&mut app);
    assert!(
        !shown.contains('▾') && shown.contains("▸ you · 2h ago · resolved · Old four"),
        "{shown}"
    );
}

/// A click on a thread's header row folds or unfolds it; a click in
/// its body only focuses it.
#[test]
fn a_click_on_a_thread_header_folds_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    let shown = screen(&mut app);
    let header = shown
        .lines()
        .position(|l| l.contains("▾ you · 2h ago ─"))
        .expect("an open thread's header");
    let row = |shown: &str| shown.lines().nth(header).unwrap_or_default().to_owned();
    let click = |app: &mut App, y: usize| {
        app.event(Event::Input(Input::Click {
            x: 60,
            y: u16::try_from(y).expect("row"),
        }));
    };
    click(&mut app, header + 1);
    assert!(
        row(&screen(&mut app)).contains('▾'),
        "a body click doesn't fold"
    );
    assert!(app.doc.focused().is_some(), "a body click focuses");
    click(&mut app, header);
    let shown = screen(&mut app);
    assert!(row(&shown).contains('▸'), "{shown}");
    click(&mut app, header);
    let shown = screen(&mut app);
    assert!(row(&shown).contains('▾'), "{shown}");
}

/// `c` in the tree keeps only the files with comments, and `f` keeps
/// to them; `esc` shows every file again.
#[test]
fn the_tree_keeps_the_files_with_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    screen(&mut app);
    press(&mut app, Key::Tab);
    key(&mut app, 'c');
    let shown = screen(&mut app);
    assert!(shown.contains("Files · 2/12"), "{shown}");
    assert!(
        !shown.contains("zz0.rs") && shown.contains("zz9.rs"),
        "{shown}"
    );
    key(&mut app, 'f');
    assert_eq!(app.doc.document.pos.file, 11);
    press(&mut app, Key::Esc);
    assert!(screen(&mut app).contains("zz0.rs"));
}

/// A paste is never read as keys: outside the text box it's dropped,
/// but the search being typed takes its first line.
#[test]
fn a_paste_outside_the_box_is_never_keys() {
    let mut app = app(100, 20);
    let before = app.doc.document.pos;
    let paste = |text: &str| Event::Input(Input::Paste(text.to_owned()));
    assert_eq!(app.event(paste("qjjj")), None);
    assert_eq!(app.doc.document.pos, before);
    key(&mut app, '/');
    app.event(paste("fifty\nq"));
    let typed = app.search.as_ref().map(|s| s.needle.text.clone());
    assert_eq!(typed.as_deref(), Some("fifty"));
}

fn placed(app: &App, root: &str) -> bool {
    (app.comments.as_ref())
        .and_then(|c| c.threads(FILE.as_bytes()))
        .is_some_and(|threads| threads.iter().any(|t| t.root == root))
}

/// When another program changes the file, its threads are placed again
/// and its notice shows.
#[test]
fn a_changed_comments_file_is_read_again_with_its_notice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    screen(&mut app);
    assert!(placed(&app, "p1"));
    review::apply(
        &path,
        vec![
            Op::Rm(Target::Id("p1".to_owned())),
            Op::Notice(Some("comments as of 10:42".to_owned())),
        ],
    )
    .expect("applies");
    app.event(Event::Comments);
    let shown = screen(&mut app);
    assert!(shown.contains("comments as of 10:42"), "{shown}");
    assert!(!placed(&app, "p1") && placed(&app, "d1"));
}

/// A review plugin's progress reads the comments file again, without
/// waiting on the file watcher: it writes as it goes.
#[test]
fn a_review_runs_progress_reads_the_comments_file_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    screen(&mut app);
    assert!(placed(&app, "p1"));
    review::apply(&path, vec![Op::Rm(Target::Id("p1".to_owned()))]).expect("applies");
    let progress = Said::Progress("page 2".to_owned());
    app.event(Event::Review {
        id: 1,
        said: progress,
    });
    assert!(!placed(&app, "p1") && placed(&app, "d1"));
}

/// Only a review session's file has drafts: your comments not posted
/// in it. In a local review nothing is posted, so nothing is a draft,
/// and an agent's comment in a session is left for you, marked, rather
/// than posted under your name.
#[test]
fn drafts_are_yours_in_a_review_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    review::apply(
        &path,
        vec![Op::Add {
            comment: Box::new(review::Comment {
                path: FILE.to_owned(),
                text: "From the agent".to_owned(),
                author: "claude".to_owned(),
                ..review::Comment::default()
            }),
            reply_to: Some(Target::Id("d1".to_owned())),
        }],
    )
    .expect("applies");
    app.event(Event::Comments);
    let shown = screen(&mut app);
    assert!(!shown.contains("draft") && !shown.contains('◆'), "{shown}");
    assert!(!shown.contains("not posted"), "{shown}");
    assert_eq!(app.comments.as_ref().map(Comments::counts), Some((6, 0)));

    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(&mut app, prepare, Said::Done(Ok(())));
    let shown = screen(&mut app);
    assert!(shown.contains("▾ you · 2h ago · draft"), "{shown}");
    assert!(shown.contains("claude · just now · not posted"), "{shown}");
    assert_eq!(app.comments.as_ref().map(Comments::counts), Some((6, 5)));
    assert_eq!(
        app.sending(true).comments,
        5,
        "the agent's reply isn't sent"
    );

    // `a` on its thread makes it yours: a draft, sent with the rest.
    app.reviewing.job = None;
    app.follow_thread("d1");
    key(&mut app, 'a');
    assert!(status(&mut app).contains("now yours: 1 draft"));
    assert_eq!(app.sending(true).comments, 6);
    let authors: Vec<String> = (read_comments(&path).comments.into_iter())
        .map(|c| c.author)
        .collect();
    assert!(!authors.contains(&"claude".to_owned()), "{authors:?}");
}

/// The local review's file holds every branch's threads: the viewer
/// shows the current branch's and those on none, a new one goes on the
/// current branch, and the comments panel lists the others folded,
/// where `b` shows them and `V` brings one here.
#[test]
fn local_threads_keep_to_their_branch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let json = format!(
        r#"{{"version":1,"comments":[
{{"id":"h1","path":"{FILE}","side":"new","start":{{"old":null,"new":2}},"end":{{"old":null,"new":2}},"text":"Here","author":"","created":"2026-11-17T10:00:00Z","branch":"main","anchor":{{"before":[],"lines":["line 2"],"after":[]}}}},
{{"id":"o1","path":"{FILE}","side":"new","start":{{"old":null,"new":2}},"end":{{"old":null,"new":2}},"text":"Elsewhere","author":"","created":"2026-11-17T10:00:00Z","branch":"topic","anchor":{{"before":[],"lines":["line 2"],"after":[]}}}},
{{"id":"o2","path":"{FILE}","text":"Its reply","author":"","created":"2026-11-17T10:01:00Z","branch":"topic","in_reply_to":"o1"}},
{{"id":"n1","path":"{FILE}","text":"Everywhere","author":"","created":"2026-11-17T10:00:00Z"}}
]}}"#
    );
    std::fs::write(&path, json).expect("write");
    let mut app = app(200, 30);
    app.set_branch(Some("main".to_owned()));
    app.set_comments(path.clone());
    let shown = screen(&mut app);
    assert!(
        shown.contains("Here") && shown.contains("Everywhere"),
        "{shown}"
    );
    assert!(!shown.contains("Elsewhere"), "{shown}");
    assert_eq!(app.comments.as_ref().map(Comments::counts), Some((2, 0)));

    key(&mut app, 'c');
    typed(&mut app, "New one");
    press(&mut app, Key::Esc);
    let added = read_comments(&path)
        .comments
        .into_iter()
        .find(|c| c.text == "New one")
        .expect("saved");
    assert_eq!(added.branch.as_deref(), Some("main"));

    key(&mut app, 'i');
    let listed = left(&mut app);
    assert!(
        listed.contains("1 thread on other branches · b lists them"),
        "{listed}"
    );
    assert!(!listed.contains("Elsewhere"), "{listed}");
    key(&mut app, 'b');
    let listed = left(&mut app);
    assert!(
        listed.contains("Other branches · V moves one here") && listed.contains("topic"),
        "{listed}"
    );
    for _ in 0..10 {
        if selected(&app) == "o1" {
            break;
        }
        press(&mut app, Key::Down);
    }
    app.message = None;
    app.dirty = true;
    assert!(
        status(&mut app).contains("V move here"),
        "{}",
        status(&mut app)
    );
    // `B` is keep both's, in a conflict, and moves nothing.
    key(&mut app, 'B');
    let kept = read_comments(&path);
    assert!(
        kept.comments
            .iter()
            .filter(|c| c.id.starts_with('o'))
            .all(|c| c.branch.as_deref() == Some("topic"))
    );
    key(&mut app, 'V');
    let moved = read_comments(&path);
    assert!(
        moved
            .comments
            .iter()
            .filter(|c| c.id.starts_with('o'))
            .all(|c| c.branch.as_deref() == Some("main")),
        "the thread, its reply too"
    );
    press(&mut app, Key::Esc);
    assert!(screen(&mut app).contains("Elsewhere"));
}

/// A checkout moves the comments shown to the new branch's, and new
/// ones go on it.
#[test]
fn a_checkout_shows_the_new_branchs_threads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let thread = |id: &str, text: &str, branch: &str| {
        format!(
            r#"{{"id":"{id}","path":"{FILE}","text":"{text}","author":"","created":"2026-11-17T10:00:00Z","branch":"{branch}"}}"#
        )
    };
    let json = format!(
        r#"{{"version":1,"comments":[{},{}]}}"#,
        thread("h1", "On main", "main"),
        thread("t1", "On topic", "topic")
    );
    std::fs::write(&path, json).expect("write");
    let mut app = app(200, 30);
    app.set_branch(Some("main".to_owned()));
    app.set_comments(path.clone());
    let shown = screen(&mut app);
    assert!(
        shown.contains("On main") && !shown.contains("On topic"),
        "{shown}"
    );
    app.event(Event::Branch(Some("topic".to_owned())));
    let shown = screen(&mut app);
    assert!(
        shown.contains("On topic") && !shown.contains("On main"),
        "{shown}"
    );
    key(&mut app, 'C');
    typed(&mut app, "New one");
    press(&mut app, Key::Esc);
    let added = (read_comments(&path).comments.into_iter())
        .find(|c| c.text == "New one")
        .expect("saved");
    assert_eq!(added.branch.as_deref(), Some("topic"));
}

/// On the review plain dffy found, a new comment is the working tree's:
/// it goes to the local review's file, on the branch, until posting
/// moves it.
#[test]
fn on_a_detected_review_new_comments_go_to_the_local_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (pr, local) = (
        dir.path().join("comments.json"),
        dir.path().join("local.json"),
    );
    std::fs::write(&pr, PR_FILE).expect("write");
    let mut app = app(100, 30);
    described(&mut app, DESCRIBED);
    app.set_branch(Some("feature".to_owned()));
    app.set_local_comments(Some(local.clone()));
    let (id, _) = plugin_run(Some(app.detect_review("test".to_owned())));
    let (prepare, ..) = plugin_run(said(
        &mut app,
        id,
        Said::Detected(Some(session(&dir, HEAD, None))),
    ));
    said(&mut app, prepare, Said::Session(session(&dir, HEAD, None)));
    app.set_comments(pr.clone());
    screen(&mut app);
    key(&mut app, 'c');
    typed(&mut app, "Fix this");
    press(&mut app, Key::Esc);
    let written = read_comments(&local).comments;
    assert_eq!(written.len(), 1, "{written:?}");
    assert_eq!(written[0].branch.as_deref(), Some("feature"));
    assert!(
        !read_comments(&pr)
            .comments
            .iter()
            .any(|c| c.text == "Fix this")
    );
    assert!(screen(&mut app).contains("draft · local"));
}

/// "Suggest change" opens the box with the lines as a suggestion to
/// edit, the cursor above it; saved, the lines go in `suggestion`, and
/// `e` edits both.
#[test]
fn a_suggested_change_is_saved_as_the_comments_suggestion() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = app(100, 30);
    app.set_comments(path.clone());
    screen(&mut app);
    key(&mut app, 'S');
    let text = app.doc.writing().map(|w| w.text.text().to_owned());
    assert_eq!(
        text.as_deref(),
        Some("\n\n```suggestion\nline 1\n```"),
        "{}",
        screen(&mut app)
    );
    typed(&mut app, "Say one");
    press(&mut app, Key::Down);
    press(&mut app, Key::Down);
    press(&mut app, Key::Down);
    press(&mut app, Key::End);
    typed(&mut app, " (one)");
    press(&mut app, Key::Esc);
    let saved = read_comments(&path).comments;
    assert_eq!(
        (saved[0].text.as_str(), saved[0].suggestion.as_deref()),
        ("Say one", Some("line 1 (one)"))
    );
    assert!(screen(&mut app).contains("suggested change:"));
    key(&mut app, 'e');
    let text = app.doc.writing().map(|w| w.text.text().to_owned());
    assert_eq!(
        text.as_deref(),
        Some("Say one\n\n```suggestion\nline 1 (one)\n```")
    );
}

/// A comment written in the editor whose save fails opens in the box
/// again where the diff shows its place, else comes back to keep.
#[test]
fn text_from_the_editor_is_never_lost() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = app(100, 30);
    app.set_comments(path.clone());
    std::fs::write(&path, r#"{"version":2,"comments":[]}"#).expect("write");
    screen(&mut app);
    key(&mut app, ' ');
    assert!(matches!(
        press(&mut app, Key::Char('E')),
        Some(Action::Compose(_))
    ));
    assert_eq!(app.composed(Some("Written there".to_owned())), None);
    let text = app.doc.writing().map(|w| w.text.text().to_owned());
    assert_eq!(text.as_deref(), Some("Written there"), "back in the box");

    press(&mut app, Key::Esc);
    press(&mut app, Key::Esc);
    app.composing = Some(Compose::Reply("gone".to_owned()));
    assert_eq!(
        app.composed(Some("A reply".to_owned())),
        Some("A reply".to_owned()),
        "no box for a thread not shown: the caller keeps it"
    );
}

/// A signal to stop saves the box's text, as `ctrl+c` does; leaving a
/// review with drafts not posted says so.
#[test]
fn an_interrupt_saves_the_box_and_quitting_names_unposted_drafts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    screen(&mut app);
    key(&mut app, 'C');
    typed(&mut app, "Half written");
    app.interrupted();
    assert!(
        read_comments(&path)
            .comments
            .iter()
            .any(|c| c.text == "Half written")
    );
    assert_eq!(app.unposted_note(), None, "not a review");
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    assert_eq!(
        app.unposted_note().as_deref(),
        Some("PR #7: 6 drafts not posted")
    );
}

/// A review the host closed or merged says so in the status bar, and
/// the dialog warns before posting on it.
#[test]
fn a_merged_review_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    let merged = Session {
        state: Some(plugin::review::State::Merged),
        ..session(&dir, HEAD, None)
    };
    app.start_review("test".to_owned(), merged);
    app.reviewing.job = None;
    app.message = None;
    assert!(
        status(&mut app).contains("PR #7 · merged"),
        "{}",
        status(&mut app)
    );
    press(&mut app, Key::Char('P'));
    assert!(screen(&mut app).contains("! PR #7 is merged"));
}

/// A `pull` that brings threads says how many, and `m` goes to them
/// first; the session pointer names the review session.
#[test]
fn an_update_names_the_new_threads_and_m_goes_to_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    assert_eq!(
        app.session_pointer(),
        Some((path.clone(), "7".to_owned(), HEAD.to_owned()))
    );
    let (pull, ..) = plugin_run(press(&mut app, Key::Char('p')));
    said(&mut app, pull, Said::Session(session(&dir, HEAD, None)));
    review::apply(
        &path,
        vec![Op::Upsert {
            fields: [
                ("ref", "github:9"),
                ("path", FILE),
                ("text", "Brand new"),
                ("author", "octocat"),
            ]
            .map(|(k, v)| (k.to_owned(), plugin::json::Value::String(v.to_owned())))
            .into(),
            reply_to: None,
        }],
    )
    .expect("applies");
    said(&mut app, pull, Said::Done(Ok(())));
    assert!(
        status(&mut app).contains("1 new comment · m goes to them"),
        "{}",
        status(&mut app)
    );
    key(&mut app, 'm');
    let new = read_comments(&path)
        .comments
        .into_iter()
        .find(|c| c.text == "Brand new")
        .expect("imported");
    assert_eq!(app.doc.focused().map(|t| t.root.clone()), Some(new.id));
}

/// A plugin's words are escaped where they're drawn: the status bar shows
/// a control character as `^[`, and the terminal's report carries none.
#[test]
fn a_plugins_words_are_escaped_where_theyre_drawn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 20);
    described(&mut app, DESCRIBED);
    let id = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(
        &mut app,
        id,
        Said::Done(Err("no \u{1b}[2J access".to_owned())),
    );
    let shown = screen(&mut app);
    assert!(shown.contains("no ^[[2J access"), "{shown}");
    let report = app.status_report().expect("a report");
    assert!(!report.windows(3).any(|w| w == b"\x1b[2"), "{report:?}");
}

/// A reply whose thread leaves the file while it's written, as when an
/// import removes it, keeps its text: the box writes a comment on the
/// file from then on, and says so.
#[test]
fn a_reply_whose_thread_goes_keeps_its_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    key(&mut app, 'm');
    key(&mut app, 'm');
    assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("d1"));
    key(&mut app, 'r');
    typed(&mut app, "Keep me");
    review::apply(&path, vec![Op::Rm(Target::Id("d1".to_owned()))]).expect("removed");
    app.event(Event::Comments);
    let writes = app
        .doc
        .writing()
        .map(|w| (w.at.clone(), w.text.text().to_owned()));
    assert_eq!(writes, Some((Writes::File, "Keep me".to_owned())));
    let shown = screen(&mut app);
    assert!(shown.contains("Keep me"), "{shown}");
    assert!(shown.contains("is gone"), "{shown}");
    press(&mut app, Key::Esc);
    let kept = read_comments(&path);
    let saved = kept
        .comments
        .iter()
        .find(|c| c.text == "Keep me")
        .expect("saved");
    assert!(
        saved.in_reply_to.is_none() && saved.end.is_none(),
        "{saved:?}"
    );
}

use super::support::*;

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
                prepare.scopes(file.diff(), slot);
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

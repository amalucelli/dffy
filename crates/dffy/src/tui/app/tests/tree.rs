use super::support::*;

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

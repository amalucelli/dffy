use super::support::*;

#[test]
fn every_key_works_at_forty_by_twelve() {
    let mut app = app(40, 12);
    for c in "nnNNffFFxXXjkgG sbW?q?TT/ab\u{7f}vjjskyYv<>HL0WvG+-lsnxXXsl".chars() {
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

/// On the old side in file view, `v` selects the old side's lines and `s`
/// while selecting keeps to them, saying why.
#[test]
fn file_view_selects_the_side_read() {
    let mut app = app(100, 20);
    app.draw();
    for c in "lsv".chars() {
        key(&mut app, c);
    }
    assert_eq!(
        app.doc.selection.as_ref().map(|s| s.side),
        Some(document::Side::Old)
    );
    key(&mut app, 's');
    let said = status(&mut app);
    assert!(
        said.contains("file view copies the version shown"),
        "{said}"
    );
    assert_eq!(
        app.doc.selection.as_ref().map(|s| s.side),
        Some(document::Side::Old)
    );
}

/// In file view `x` opens the first change on screen in place and folds it
/// again, saying so; `X` switches whole files for when file view is off,
/// remembered, and says that too.
#[test]
fn file_view_says_what_x_and_shift_x_do() {
    let mut app = app(100, 20);
    app.draw();
    key(&mut app, 'l');
    key(&mut app, 'x');
    let said = status(&mut app);
    assert!(said.contains("1 line removed shown here"), "{said}");
    assert_eq!(app.doc.document.opened(0), [0]);
    key(&mut app, 'x');
    let said = status(&mut app);
    assert!(said.contains("1 line removed hidden"), "{said}");
    assert_eq!(app.doc.document.opened(0), [] as [usize; 0]);
    key(&mut app, 'X');
    let said = status(&mut app);
    assert!(
        said.contains("whole files applies once file view is off"),
        "{said}"
    );
    assert!(app.doc.document.full);
    let remembered = app.pending_remembered();
    assert!(
        remembered.contains(&("context", "full".to_owned())),
        "{remembered:?}"
    );
    key(&mut app, 'l');
    assert_eq!(app.doc.document.reading(), None);
    assert!(app.doc.document.full, "whole files once file view is off");
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

/// Outside the text box `ctrl+j` is `enter`, as the line feed some
/// terminals send for it: it ends the search, the tree's filter and an
/// overlay's pick as `enter` does. In the text box it starts a line.
#[test]
fn ctrl_j_is_enter_outside_the_text_box() {
    let mut app = app(100, 20);
    app.draw();
    let ctrl_j = |app: &mut App| app.event(Event::Input(Input::Key(Key::Char('j'), Mods::CTRL)));
    typed(&mut app, "/line");
    ctrl_j(&mut app);
    assert!(app.search.as_ref().is_some_and(|s| !s.editing));
    esc(&mut app);
    app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
    typed(&mut app, "/zz");
    ctrl_j(&mut app);
    assert!(!app.tree.editing);
    assert_eq!(app.tree.filter(), "zz");
    esc(&mut app);
    esc(&mut app);
    typed(&mut app, "  ");
    assert!(app.commands.is_some());
    ctrl_j(&mut app);
    assert!(app.commands.is_none(), "the command panel ran its first");
    assert!(app.picker.is_some(), "Find file…");
    ctrl_j(&mut app);
    assert!(app.picker.is_none(), "the picker opened its file");

    let path = tempfile::tempdir().expect("tempdir");
    app.set_comments(path.path().join("comments.json"));
    screen(&mut app);
    key(&mut app, 'c');
    typed(&mut app, "one");
    ctrl_j(&mut app);
    typed(&mut app, "two");
    let text = app.doc.writing().map(|w| w.text.text().to_owned());
    assert_eq!(text.as_deref(), Some("one\ntwo"));
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
    has(&mut app, "│ ctrl+c ctrl+q quit General");
    app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
    typed(&mut app, "/help");
    has(&mut app, "│ f1 show this help General");
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    app.event(Event::Input(Input::Key(Key::F(1), Mods::NONE)));
    assert!(!screen(&mut app).contains("this help"));
    assert_eq!(
        app.event(Event::Input(Input::Key(Key::Char('q'), Mods::CTRL))),
        Some(Action::Quit)
    );
}

/// A terminal opened says so; one that couldn't be leaves a `cd` to the
/// working tree on the clipboard, saying why.
#[test]
fn a_terminal_that_cant_open_leaves_a_cd_to_paste() {
    let mut app = app(100, 20);
    let opened = Event::Terminal {
        dir: "/src/my repo".into(),
        opened: Ok("opened a tmux window".to_owned()),
    };
    assert_eq!(app.event(opened), None);
    assert!(status(&mut app).contains("opened a tmux window"));
    let failed = Event::Terminal {
        dir: "/src/my repo".into(),
        opened: Err("tmux failed (exit status: 1)".to_owned()),
    };
    assert_eq!(
        app.event(failed),
        Some(Action::Copy {
            text: b"cd '/src/my repo'".to_vec(),
            note: "tmux failed (exit status: 1); copied cd '/src/my repo'".to_owned(),
        })
    );
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

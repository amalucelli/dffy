use super::support::*;

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

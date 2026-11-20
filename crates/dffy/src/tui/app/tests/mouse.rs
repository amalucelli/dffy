use super::support::*;

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

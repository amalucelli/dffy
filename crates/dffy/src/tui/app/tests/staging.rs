use dffy_core::git::Applied;
use dffy_core::view::Staging;

use super::support::*;

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
        note(&mut app).as_deref(),
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

/// In file view, `A` with nothing selected stages the whole hunk, both its
/// removed and added lines, and says so; with lines selected it's refused,
/// since picking lines needs both sides.
#[test]
fn file_view_stages_whole_hunks_and_refuses_a_selection() {
    let mut app = app(100, 20);
    app.set_uncommitted(Some(Uncommitted::Unstaged));
    app.draw();
    key(&mut app, 'l');
    app.draw();
    let Some(Action::EditLines {
        diff: shown,
        picked,
        edit: Edit::Stage,
    }) = press(&mut app, Key::Char('A'))
    else {
        panic!("A stages");
    };
    assert_eq!(picked, Picked::hunk(&shown.hunks[0]));
    assert!(
        !picked.old.is_empty() && !picked.new.is_empty(),
        "{picked:?}"
    );
    app.event(Event::Edited(Ok("staged 2 lines".to_owned())));
    let said = status(&mut app);
    assert!(said.contains("staged 2 lines: the whole hunk"), "{said}");
    for c in "vj".chars() {
        key(&mut app, c);
    }
    assert_eq!(press(&mut app, Key::Char('A')), None);
    let said = status(&mut app);
    assert!(
        said.contains("staging lines needs both versions; x opens the"),
        "{said}"
    );
}

/// In file view, a tap on a mark row of its own, lines removed at a hunk's
/// end, aims `A` and `D` at that hunk, not the first on screen.
#[test]
fn file_view_stages_the_hunk_of_a_tapped_mark() {
    let mut fd = diff("src/a.rs");
    let old = (1..=60).fold(String::new(), |out, i| out + &format!("line {i}\n"));
    let new = (1..=58)
        .map(|i| match i {
            45 => "line forty-five\n".to_owned(),
            i => format!("line {i}\n"),
        })
        .collect::<String>();
    (fd.old, fd.new) = (Text::new(old.into()), Text::new(new.into()));
    fd.hunks = hunks(&fd.old, &fd.new, &Options::default());
    let mut app = app_of(&Config::default(), "working tree", fd, (100, 40));
    app.set_uncommitted(Some(Uncommitted::Unstaged));
    app.draw();
    key(&mut app, 'l');
    key(&mut app, 'G');
    let shown = screen(&mut app);
    let y = shown
        .lines()
        .position(|l| l.contains("2 lines removed · x shows them"))
        .unwrap_or_else(|| panic!("{shown}"));
    // A second click of a double click toggles nothing.
    assert_eq!(app.doc.click(y - 1, false), None);
    assert!(screen(&mut app).contains("2 lines removed · x shows them"));
    let y = u16::try_from(y).expect("on screen");
    app.event(Event::Input(Input::Click { x: 60, y }));
    app.event(Event::Input(Input::Release { x: 60, y }));
    let Some(Action::EditLines {
        diff: staged,
        picked,
        ..
    }) = press(&mut app, Key::Char('A'))
    else {
        panic!("A stages");
    };
    assert_eq!(staged.hunks.len(), 2);
    assert_eq!(picked, Picked::hunk(&staged.hunks[1]));
    app.event(Event::Edited(Ok("staged 2 lines".to_owned())));
    // The tap opened the run in place; a tap on its rule folds it again.
    let shown = screen(&mut app);
    let y = shown
        .lines()
        .position(|l| l.contains("2 lines removed · x hides them"))
        .unwrap_or_else(|| panic!("{shown}"));
    let y = u16::try_from(y).expect("on screen");
    app.event(Event::Input(Input::Click { x: 60, y }));
    app.event(Event::Input(Input::Release { x: 60, y }));
    assert!(screen(&mut app).contains("2 lines removed · x shows them"));
    let Some(Action::PlanDiscard { diff, picked }) = press(&mut app, Key::Char('D')) else {
        panic!("D plans");
    };
    assert_eq!(picked, Picked::hunk(&diff.hunks[1]));
}

/// In file view, `x` opens the change of the edited line tapped, not the
/// one the top row is in.
#[test]
fn file_view_x_opens_the_tapped_change() {
    let mut fd = diff("src/a.rs");
    let old = (1..=60).fold(String::new(), |out, i| out + &format!("line {i}\n"));
    let new = (1..=58)
        .map(|i| match i {
            45 => "line forty-five\n".to_owned(),
            57 => "line fifty-seven\n".to_owned(),
            i => format!("line {i}\n"),
        })
        .collect::<String>();
    (fd.old, fd.new) = (Text::new(old.into()), Text::new(new.into()));
    fd.hunks = hunks(&fd.old, &fd.new, &Options::default());
    let mut app = app_of(&Config::default(), "working tree", fd, (100, 20));
    app.draw();
    key(&mut app, 'l');
    key(&mut app, 'G');
    let shown = screen(&mut app);
    let rows: Vec<&str> = shown.lines().collect();
    // The top row is the first change's context.
    assert!(rows[1].contains("│ line 42 "), "{shown}");
    let y = (rows.iter())
        .position(|l| l.contains("┃ line fifty-seven "))
        .unwrap_or_else(|| panic!("{shown}"));
    let y = u16::try_from(y).expect("on screen");
    app.event(Event::Input(Input::Click { x: 60, y }));
    app.event(Event::Input(Input::Release { x: 60, y }));
    key(&mut app, 'x');
    assert_eq!(app.doc.document.opened(0), [1]);
    let shown = screen(&mut app);
    assert!(shown.contains("3 lines removed · x hides them"), "{shown}");
}

/// In file view, an edited line has no rule, only its bar; `x` opens its
/// change in place, showing both sides as the diff does: the removed line
/// inline where it was, without a bar, and a selection there stages as in
/// the diff.
#[test]
fn file_view_opens_a_change_in_place_and_stages_lines_there() {
    let mut app = app(100, 20);
    app.set_uncommitted(Some(Uncommitted::Unstaged));
    app.draw();
    key(&mut app, 'l');
    let shown = screen(&mut app);
    assert!(
        !shown.contains("line 4\n") && !shown.contains("line 4 "),
        "{shown}"
    );
    // An edited line has no rule: its bar marks it.
    assert!(shown.contains("┃ line four"), "{shown}");
    assert!(!shown.contains("line removed"), "{shown}");
    key(&mut app, 'x');
    let shown = screen(&mut app);
    let rows: Vec<&str> = shown.lines().collect();
    let removed = (rows.iter())
        .position(|r| r.contains("│ line 4 "))
        .unwrap_or_else(|| panic!("{shown}"));
    assert!(!rows[removed].contains("┃ line"), "{shown}");
    assert!(rows[removed + 1].contains("┃ line four"), "{shown}");
    assert!(rows[removed - 1].contains("line 3"), "{shown}");
    assert!(shown.contains("1 line removed · x hides them"), "{shown}");
    let said = status(&mut app);
    assert!(
        said.contains("1 line removed shown here · x hides them"),
        "{said}"
    );
    for c in "vjjjj".chars() {
        key(&mut app, c);
    }
    let Some(Action::EditLines { picked, .. }) = press(&mut app, Key::Char('A')) else {
        panic!("A stages the selection: {}", status(&mut app));
    };
    assert_eq!(
        (picked.old.contains(&3), picked.new.contains(&3)),
        (true, true),
        "{picked:?}"
    );
    app.event(Event::Edited(Ok("staged 1 line".to_owned())));
    key(&mut app, 'x');
    let shown = screen(&mut app);
    assert!(!shown.contains("── 1 line removed"), "{shown}");
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

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

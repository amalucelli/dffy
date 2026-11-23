use super::support::*;
use crate::control::viewer::{Command, Inbound, Reply};

const ON_DISK: Side = Side::Worktree {
    mode: Mode::File,
    id: None,
};

fn back(app: &mut App) {
    press(app, Key::Backspace);
}

fn forward(app: &mut App) {
    key(app, '\\');
}

/// The status bar's message in full, which a narrow bar cuts.
fn message(app: &App) -> String {
    (app.message.as_ref()).map_or_else(String::new, |(m, _)| m.clone())
}

/// `o`, `query`, `enter`: the picker's best match for `query`.
fn pick(app: &mut App, query: &str) {
    key(app, 'o');
    typed(app, query);
    press(app, Key::Enter);
}

/// `view/open` of `path` at `line`, from the control API.
fn view_open(app: &mut App, path: &str, line: Option<usize>) {
    let (reply, _answer) = Reply::channel();
    let command = Command::Open {
        path: path.to_owned(),
        line,
        side: document::Side::New,
    };
    app.event(Event::Control(Inbound::Command(command, reply)));
}

/// [`app`] whose sides are a commit and the working tree.
fn sided() -> App {
    let mut app = app(100, 20);
    app.set_snapshots(Some([tree("ab"), Snapshot::Worktree]));
    app
}

fn tree(hex: &str) -> Snapshot {
    let hex = hex.repeat(20);
    Snapshot::Tree(dffy_core::git::ObjectId::from_hex(hex.as_bytes()).expect("hex"))
}

/// `o` picks the second file, from well down the first; back shows the
/// first with the line that was at the top at the top again, forward the
/// second as it was left, and going round again doesn't drift.
#[test]
fn back_and_forward_return_to_the_lines_jumped_from() {
    let mut app = app(100, 20);
    screen(&mut app);
    key(&mut app, 'X');
    for _ in 0..12 {
        key(&mut app, 'j');
    }
    screen(&mut app);
    let left = app.doc.top_lines();
    assert_ne!(left, [Some(0), Some(0)], "away from the top");
    key(&mut app, 'o');
    typed(&mut app, "zz");
    press(&mut app, Key::Enter);
    app.event(Event::Diffed(diffed(&app, 1, diff("src/zz.rs"))));
    screen(&mut app);
    assert_eq!(app.doc.document.pos.file, 1);
    let there = app.doc.top_lines();

    back(&mut app);
    let line = left[1].expect("a new line") + 1;
    assert_eq!(message(&app), format!("back to {FILE}:{line}"));
    screen(&mut app);
    assert_eq!((app.doc.document.pos.file, app.doc.top_lines()), (0, left));

    forward(&mut app);
    assert!(
        message(&app).starts_with("forward to src/zz.rs:"),
        "{}",
        message(&app)
    );
    screen(&mut app);
    assert_eq!((app.doc.document.pos.file, app.doc.top_lines()), (1, there));

    back(&mut app);
    screen(&mut app);
    assert_eq!((app.doc.document.pos.file, app.doc.top_lines()), (0, left));
    forward(&mut app);
    screen(&mut app);
    assert_eq!((app.doc.document.pos.file, app.doc.top_lines()), (1, there));
}

/// Moving through the tree shows each file it passes; opening one from
/// there goes back to where the tree was entered, not to a file passed.
#[test]
fn back_from_a_file_opened_in_the_tree_skips_the_files_passed() {
    let mut app = app(120, 24);
    for i in 0..4 {
        app.event(Event::Enumerated(diff(&format!("src/zz{i}.rs")).change));
    }
    screen(&mut app);
    press(&mut app, Key::Tab);
    let mut passed = Vec::new();
    for _ in 0..8 {
        key(&mut app, 'j');
        passed.push(app.doc.document.pos.file);
    }
    passed.dedup();
    assert!(
        passed.len() > 2,
        "the tree showed files on its way: {passed:?}"
    );
    press(&mut app, Key::Enter);
    let opened = app.doc.document.pos.file;
    assert_ne!(opened, 0);

    back(&mut app);
    assert_eq!(app.doc.document.pos.file, 0);
    back(&mut app);
    assert_eq!(message(&app), "nothing to go back to");
    assert_eq!(app.doc.document.pos.file, 0);
    // In and out of the tree on the same file is no jump.
    press(&mut app, Key::Tab);
    press(&mut app, Key::Tab);
    back(&mut app);
    assert_eq!(message(&app), "nothing to go back to");

    // Leaving the tree for the diff on another file is a jump from where
    // the tree took the keys, as enter is; the file it showed is where
    // it's entered from next.
    forward(&mut app);
    let up_to_another = |app: &mut App| {
        let from = app.doc.document.pos.file;
        for _ in 0..8 {
            key(app, 'k');
            if app.doc.document.pos.file != from {
                return app.doc.document.pos.file;
            }
        }
        panic!("the tree showed no other file");
    };
    press(&mut app, Key::Tab);
    let left = up_to_another(&mut app);
    press(&mut app, Key::Tab);
    press(&mut app, Key::Tab);
    up_to_another(&mut app);
    press(&mut app, Key::Enter);
    back(&mut app);
    assert_eq!(app.doc.document.pos.file, left);
    back(&mut app);
    assert_eq!(app.doc.document.pos.file, opened);
    back(&mut app);
    assert_eq!(app.doc.document.pos.file, 0);
}

/// A place whose file was closed is skipped, and said.
#[test]
fn back_skips_a_place_whose_file_is_gone() {
    let mut app = app(100, 20);
    screen(&mut app);
    app.other_found("notes.txt".into(), document::Side::New, Ok(Some(ON_DISK)));
    let notes = app.doc.document.pos.file;
    assert!(app.doc.document.files()[notes].is_unchanged());
    screen(&mut app);
    key(&mut app, 'o');
    typed(&mut app, "zz");
    press(&mut app, Key::Enter);
    assert_eq!(app.doc.document.pos.file, 1);
    app.relooked(vec![("notes.txt".into(), document::Side::New, Ok(None))]);
    assert!(app.unchanged_file("notes.txt".into()).is_none());

    back(&mut app);
    assert_eq!(
        message(&app),
        format!("back to {FILE}:1 · skipped 1 no longer shown")
    );
    assert_eq!(app.doc.document.pos.file, 0);
}

/// The control API's `view/open` is a jump too.
#[test]
fn back_from_a_file_the_control_api_opened() {
    let mut app = app(100, 20);
    screen(&mut app);
    let (reply, _answer) = Reply::channel();
    let command = Command::Open {
        path: "src/zz.rs".to_owned(),
        line: None,
        side: document::Side::New,
    };
    app.event(Event::Control(Inbound::Command(command, reply)));
    assert_eq!(app.doc.document.pos.file, 1);
    back(&mut app);
    assert_eq!(app.doc.document.pos.file, 0);
}

/// The command panel's quick popup lists Back and Forward under Go while
/// there's somewhere to go, with the keys that do it outside the panel,
/// which run them from it too.
#[test]
fn the_command_panel_offers_back_and_forward_while_there_is_somewhere_to_go() {
    let mut app = app(120, 40);
    screen(&mut app);
    key(&mut app, ' ');
    let shown = screen(&mut app);
    assert!(
        !shown.contains("Back") && !shown.contains("Forward"),
        "{shown}"
    );
    esc(&mut app);
    key(&mut app, 'o');
    typed(&mut app, "zz");
    press(&mut app, Key::Enter);
    key(&mut app, ' ');
    let shown = screen(&mut app);
    assert!(
        shown.contains("backspace  Back") && !shown.contains("Forward"),
        "{shown}"
    );
    back(&mut app);
    assert!(app.commands.is_none(), "going somewhere closes the panel");
    assert_eq!(app.doc.document.pos.file, 0);
    key(&mut app, ' ');
    let shown = screen(&mut app);
    let row = shown.lines().find(|l| l.contains("Forward"));
    let keyed = row.is_some_and(|r| {
        r.split("Forward")
            .next()
            .is_some_and(|k| k.ends_with("\\  ") || k.trim_end().ends_with('\\'))
    });
    assert!(keyed && !shown.contains("Back"), "{shown}");
}

/// With nowhere to go, the keys say so.
#[test]
fn back_and_forward_with_nowhere_to_go_say_so() {
    let mut app = app(100, 20);
    screen(&mut app);
    back(&mut app);
    assert_eq!(message(&app), "nothing to go back to");
    forward(&mut app);
    assert_eq!(message(&app), "nothing to go forward to");
}

/// In the quick popup, `backspace` on a highlighted value row goes back,
/// as the row of Back shows; it doesn't step the value.
#[test]
fn backspace_in_the_quick_popup_goes_back_on_a_value_row() {
    let mut app = app(120, 40);
    screen(&mut app);
    pick(&mut app, "zz");
    key(&mut app, ' ');
    key(&mut app, 'j');
    let view = app.view;
    back(&mut app);
    assert_eq!(app.doc.document.pos.file, 0);
    assert_eq!(app.view, view, "the layout row didn't step");
}

/// A jump from the conversation, which has no place to keep, still
/// starts a new way forward.
#[test]
fn a_jump_from_the_conversation_clears_the_way_forward() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = conversing(&dir, (120, 30));
    screen(&mut app);
    let page = |app: &App| app.doc.document.files()[app.doc.document.pos.file].is_page();
    while page(&app) {
        key(&mut app, 'f');
    }
    pick(&mut app, "zz9.rs");
    back(&mut app);
    assert!(app.jumps.can_go(true));
    for _ in 0..20 {
        if page(&app) {
            break;
        }
        key(&mut app, 'F');
    }
    assert!(page(&app), "on the conversation");
    pick(&mut app, "zz0.rs");
    forward(&mut app);
    assert_eq!(message(&app), "nothing to go forward to");
}

/// Opening a file whole at its other version, while it shows, is a jump
/// from the version it showed, kept once.
#[test]
fn reopening_a_whole_file_at_its_other_version_keeps_the_version_left() {
    let mut app = sided();
    screen(&mut app);
    app.other_found("notes.txt".into(), document::Side::New, Ok(Some(ON_DISK)));
    screen(&mut app);
    app.other_found("notes.txt".into(), document::Side::Old, Ok(Some(ON_DISK)));
    back(&mut app);
    assert_eq!(
        message(&app),
        format!("back to {FILE}:1 · skipped 1 no longer shown")
    );
}

/// Opening the file already shown is no jump; going to a line in it is.
#[test]
fn opening_the_file_shown_is_no_jump_but_a_line_in_it_is() {
    let mut app = app(100, 20);
    // The whole file, which line 50 is far down.
    key(&mut app, 'X');
    screen(&mut app);
    pick(&mut app, "zz");
    back(&mut app);
    pick(&mut app, "file.rs");
    assert_eq!(app.doc.document.pos.file, 0);
    view_open(&mut app, FILE, None);
    assert!(app.jumps.can_go(true), "the way forward stays");
    assert!(!app.jumps.can_go(false), "nothing to go back to");
    view_open(&mut app, FILE, Some(49));
    screen(&mut app);
    back(&mut app);
    assert!(
        message(&app).starts_with(&format!("back to {FILE}:")),
        "{}",
        message(&app)
    );
}

/// Moving through the tree from the conversation, which has no place,
/// still keeps that the files passed aren't where it was entered from.
#[test]
fn moving_through_the_tree_from_the_conversation_keeps_no_file_passed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = conversing(&dir, (120, 30));
    screen(&mut app);
    let page = |app: &App| app.doc.document.files()[app.doc.document.pos.file].is_page();
    for _ in 0..20 {
        if page(&app) {
            break;
        }
        key(&mut app, 'F');
    }
    assert!(page(&app), "on the conversation");
    press(&mut app, Key::Tab);
    let start = app.doc.document.pos.file;
    let mut passed = 0;
    for _ in 0..12 {
        let before = app.doc.document.pos.file;
        key(&mut app, 'j');
        if app.doc.document.pos.file != before && app.doc.document.pos.file != start {
            passed += 1;
        }
        if passed == 2 {
            break;
        }
    }
    assert_eq!(passed, 2, "the tree showed two files");
    press(&mut app, Key::Enter);
    back(&mut app);
    assert_eq!(message(&app), "nothing to go back to");
}

/// A place whose version is another snapshot now isn't shown any more:
/// its line numbers are another version's.
#[test]
fn a_place_in_a_version_gone_since_is_skipped() {
    let mut app = sided();
    screen(&mut app);
    pick(&mut app, "zz");
    app.set_snapshots(Some([tree("ab"), tree("cd")]));
    back(&mut app);
    assert_eq!(
        message(&app),
        "nothing to go back to; skipped 1 no longer shown"
    );
}

/// The rows above the first line of code at the top, here the hunk's
/// header, show again as they did.
#[test]
fn back_shows_the_rows_above_the_top_line_again() {
    let mut app = app(100, 20);
    screen(&mut app);
    let top = app.doc.document.pos;
    pick(&mut app, "zz");
    back(&mut app);
    screen(&mut app);
    assert_eq!(app.doc.document.pos, top);
}

/// In file view on the newer version, a removed line left at the top has
/// no row of its own: back lands on the row that marks it.
#[test]
fn back_into_file_view_lands_a_removed_line_on_its_mark() {
    // Unified at this width: the removed `line 4` is a row of its own.
    let mut app = app(70, 20);
    key(&mut app, 'X');
    screen(&mut app);
    for _ in 0..10 {
        if app.doc.top_lines() == [Some(3), None] {
            break;
        }
        key(&mut app, 'j');
        screen(&mut app);
    }
    assert_eq!(app.doc.top_lines(), [Some(3), None]);
    pick(&mut app, "zz");
    app.event(Event::Diffed(diffed(&app, 1, diff("src/zz.rs"))));
    key(&mut app, 'l');
    assert_eq!(app.doc.document.reading(), Some(document::Side::New));
    back(&mut app);
    assert_eq!(message(&app), format!("back to {FILE}:4 in old version"));
    let shown = screen(&mut app);
    let first = shown.lines().find(|l| l.contains("line "));
    assert!(
        first.is_some_and(|l| l.contains("4 +┃ line four")),
        "{shown}"
    );
}

/// `backspace` in the tree, after it showed another file, goes back to
/// where the tree took the keys.
#[test]
fn back_from_the_tree_after_moving_returns_where_it_was_entered() {
    let mut app = app(120, 24);
    for i in 0..4 {
        app.event(Event::Enumerated(diff(&format!("src/zz{i}.rs")).change));
    }
    screen(&mut app);
    pick(&mut app, "zz2");
    let entered = app.doc.document.pos.file;
    assert_ne!(entered, 0);
    press(&mut app, Key::Tab);
    key(&mut app, 'k');
    assert_ne!(app.doc.document.pos.file, entered);
    back(&mut app);
    assert_eq!(app.doc.document.pos.file, entered);
}

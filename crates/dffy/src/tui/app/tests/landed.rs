use super::support::*;
use crate::control::viewer::{Command, Inbound, Reply};

/// `view/open` of `path` at `line` of the newer version.
fn view_open(app: &mut App, path: &str, line: usize) {
    let (reply, _answer) = Reply::channel();
    let command = Command::Open {
        path: path.to_owned(),
        line: Some(line),
        side: document::Side::New,
    };
    app.event(Event::Control(Inbound::Command(command, reply)));
}

/// The screen rows painted in the landed line's background, by their
/// screen row, as they read.
fn landed_rows(app: &mut App) -> Vec<(usize, String)> {
    screen(app);
    let bg = app.theme.style(Role::Landed).bg;
    assert!(
        bg.is_some(),
        "the default theme gives the landed line a background"
    );
    let grid = app.screen.shown_grid();
    (0..grid.height())
        .filter(|&y| grid.row(y).iter().any(|cell| cell.style().bg == bg))
        .map(|y| {
            let mut text = String::new();
            for cell in grid.row(y) {
                text.push_str(cell.text());
            }
            (y, text.trim_end().to_owned())
        })
        .collect()
}

/// A viewer of the 60-line fixture whole, `width` wide, with `indicators`.
fn whole(width: usize, indicators: Indicators) -> App {
    let fd = diff(FILE);
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(fd.change.clone()),
        (width, 24),
        indicators,
    );
    app.event(Event::Enumerated(diff("src/zz.rs").change));
    app.event(Event::EnumerationDone(None));
    app.event(Event::Diffed(Done::Diff {
        placed: None,
        file: 0,
        version: app.version(0),
        result: Ok(Box::new(loaded(fd))),
    }));
    key(&mut app, 'X');
    screen(&mut app);
    app
}

/// A jump to a line marks its row, a quarter of the pane down, with the
/// mark in the gutter, until the next key.
#[test]
fn a_jump_marks_the_line_it_lands_on_until_a_key() {
    let mut app = whole(100, Indicators::Classic);
    view_open(&mut app, FILE, 29);
    let rows = landed_rows(&mut app);
    let [(y, row)] = rows.as_slice() else {
        panic!("one row marked: {rows:?}");
    };
    assert!(row.contains("❯") && row.contains("line 30 "), "{row}");
    // Under the top border.
    assert_eq!(y - 1, app.doc.height / 4);
    key(&mut app, 'j');
    assert_eq!(landed_rows(&mut app), []);
    assert!(!screen(&mut app).contains('❯'));
}

/// A split row, a unified one and file view's mark the line's row.
#[test]
fn the_mark_is_on_the_lines_row_in_every_arrangement() {
    let mut split = whole(200, Indicators::Classic);
    view_open(&mut split, FILE, 49);
    let rows = landed_rows(&mut split);
    assert!(
        rows.len() == 1 && rows[0].1.contains("line 50") && rows[0].1.contains("line fifty"),
        "{rows:?}"
    );
    let mut unified = whole(70, Indicators::Classic);
    view_open(&mut unified, FILE, 49);
    let rows = landed_rows(&mut unified);
    assert!(
        rows.len() == 1 && rows[0].1.contains("line fifty") && !rows[0].1.contains("line 50"),
        "{rows:?}"
    );
    let mut read = whole(100, Indicators::Classic);
    key(&mut read, 'l');
    view_open(&mut read, FILE, 49);
    let rows = landed_rows(&mut read);
    assert!(
        rows.len() == 1 && rows[0].1.contains("line fifty"),
        "{rows:?}"
    );
}

/// Every screen row of a wrapped line is marked, the gutter's mark on its
/// first.
#[test]
fn a_wrapped_line_is_marked_on_all_its_rows() {
    let long = format!("line fifty {}", "and more ".repeat(30));
    let mut fd = diff(FILE);
    let new = String::from_utf8_lossy(fd.new.bytes()).replace("line fifty", &long);
    fd.new = Text::new(new.into_bytes());
    fd.hunks = hunks(&fd.old, &fd.new, &Options::default());
    let mut app = app_of(&Config::default(), "working tree", fd, (100, 24));
    key(&mut app, 'W');
    view_open(&mut app, FILE, 49);
    let rows = landed_rows(&mut app);
    assert!(rows.len() > 2, "{rows:?}");
    assert!(rows[0].1.contains('❯'), "{rows:?}");
    assert!(rows[1..].iter().all(|(_, r)| !r.contains('❯')), "{rows:?}");
}

/// Threads placed after the jump move the line down: the mark goes with
/// it.
#[test]
fn the_mark_stays_on_its_line_as_comments_are_placed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = whole(100, Indicators::Classic);
    let path = dir.path().join("comments.json");
    std::fs::write(&path, review_json("src/zz9.rs")).expect("write");
    app.set_comments(path);
    view_open(&mut app, FILE, 49);
    place(&mut app);
    let rows = landed_rows(&mut app);
    assert!(
        rows.len() == 1 && rows[0].1.contains("line fifty"),
        "{rows:?}"
    );
}

/// Back marks the line that was marked when the jump left it.
#[test]
fn back_marks_the_line_marked_when_it_was_left() {
    let mut app = whole(100, Indicators::Classic);
    view_open(&mut app, FILE, 44);
    screen(&mut app);
    key(&mut app, 'o');
    typed(&mut app, "zz");
    press(&mut app, Key::Enter);
    press(&mut app, Key::Backspace);
    let rows = landed_rows(&mut app);
    assert!(rows.len() == 1 && rows[0].1.contains("line 45"), "{rows:?}");
}

/// The marked line as each indicators style draws it, its background's
/// cells in `[…]`.
#[test]
fn the_marked_line_in_every_indicators_style() {
    for (name, indicators) in [
        ("landed_background", Indicators::Background),
        ("landed_bars", Indicators::Bars),
        ("landed_classic", Indicators::Classic),
        ("landed_none", Indicators::None),
    ] {
        let mut app = whole(100, indicators);
        view_open(&mut app, FILE, 49);
        screen(&mut app);
        let bg = app.theme.style(Role::Landed).bg;
        let grid = app.screen.shown_grid();
        let shown: Vec<String> = (0..grid.height())
            .map(|y| {
                let mut line = String::new();
                let mut on = false;
                for cell in grid.row(y) {
                    let now = cell.style().bg == bg;
                    if now != on {
                        line.push(if now { '[' } else { ']' });
                        on = now;
                    }
                    line.push_str(cell.text());
                }
                line.trim_end().to_owned()
            })
            .collect();
        insta::assert_snapshot!(name, shown.join("\n"));
    }
}

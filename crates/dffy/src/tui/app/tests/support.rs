//! The fixtures the viewer's test modules share: viewers of a few
//! files, comments files and review plugins, and the keys and events
//! they're driven with, as the loop and the workers would send them.
//! Each module takes them, and every name the viewer has, from here.

pub(super) use std::ops::ControlFlow;
pub(super) use std::path::Path;

pub(super) use dffy_core::diff::{Options, Text, Whitespace, hunks};
pub(super) use dffy_core::document::{self, Counts};
pub(super) use dffy_core::git::{ChangeSet, FileDiff, Mode, Repo, Side};
pub(super) use dffy_core::view::Attributes;

pub(super) use crate::plugin::review::{Draft, Listed, Plan, Push, Reason, Request, Said, Session};
pub(super) use crate::review::{self, Change, Op, Target};
pub(super) use crate::theme::{Ink, Role, Style};
pub(super) use crate::tui::app::draw::note;
pub(super) use crate::tui::app::*;
pub(super) use crate::tui::picker;

/// `fd` prepared as a worker does without highlighting.
pub(super) fn loaded(fd: FileDiff) -> File {
    File::prepare(crate::grammars(), None).file(fd, Whitespace::Exact, false)
}

pub(super) fn diff(path: &str) -> FileDiff {
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
pub(super) fn app(width: usize, height: usize) -> App {
    let first = diff("src/some/deeply/nested/module/file.rs");
    app_of(&Config::default(), "working tree", first, (width, height))
}

/// A viewer of `first`, diffed, and a second file, comparing what
/// `description` says.
pub(super) fn app_of(
    config: &Config,
    description: &str,
    first: FileDiff,
    size: (usize, usize),
) -> App {
    app_with(config, description, loaded(first), size)
}

/// [`app_of`] with the first file prepared already.
pub(super) fn app_with(
    config: &Config,
    description: &str,
    loaded: File,
    size: (usize, usize),
) -> App {
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
pub(super) fn screen(app: &mut App) -> String {
    app.draw();
    if place(app) {
        app.draw();
    }
    app.screen.text()
}

/// `fd` diffed for `file` as the pool answers, with the comments as
/// they are when it comes placed on it.
pub(super) fn diffed(app: &App, file: usize, fd: FileDiff) -> Done {
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
pub(super) fn place(app: &mut App) -> bool {
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

pub(super) fn key(app: &mut App, c: char) {
    app.event(Event::Input(Input::Key(Key::Char(c), Mods::NONE)));
}

pub(super) fn esc(app: &mut App) {
    app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
}

/// The status bar: the bottom row.
pub(super) fn status(app: &mut App) -> String {
    screen(app).lines().last().unwrap_or_default().to_owned()
}

pub(super) const FILES: &[(&str, usize, usize)] = &[
    (".clang-format", 12, 3),
    ("Documentation/admin-guide/kernel-parameters.txt", 1234, 56),
    (
        "drivers/net/wireless/realtek/rtw89/rtw8852c_table.c",
        54321,
        12345,
    ),
    ("kernel/sched/fair.c", 98765, 43210),
];

/// A viewer of `FILES`, all counted.
pub(super) fn files_app(width: usize, height: usize) -> App {
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

pub(super) fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        key(app, c);
    }
}

/// [`role_markup`] for any styles, each in `<label>…</>`.
pub(super) fn style_markup(app: &mut App, styles: &[(Style, String)], width: usize) -> String {
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

pub(super) const FILE: &str = "src/some/deeply/nested/module/file.rs";

/// A draft thread with a reply on new line 2, a posted comment on new
/// line 50, a resolved one on removed old line 4, a comment on the
/// whole file, one on a file not in the diff, and one on `far` whose
/// lines are gone.
pub(super) fn review_json(far: &str) -> String {
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
pub(super) fn commented(dir: &tempfile::TempDir) -> (App, PathBuf) {
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

pub(super) fn read_comments(path: &Path) -> review::Review {
    review::read(path).expect("reads")
}

pub(super) fn press(app: &mut App, key: Key) -> Option<Action> {
    app.event(Event::Input(Input::Key(key, Mods::NONE)))
}

pub(super) const BASE: &str = "aaaa111";

pub(super) const HEAD: &str = "bbbb222";

/// A plugin that calls a review `PR #7` and posts it with four events,
/// the first of which leaves it open on the host.
pub(super) const DESCRIBED: &str = r##"{"version":1,"command":"pr","noun":"pull request","nouns":"pull requests","short":"PR","id_prefix":"#","events":[{"id":"pending","key":"p","label":"pending: on the host, for you only","done":"review pending","submits":false},{"id":"comment","key":"c","label":"comment","done":"commented"},{"id":"approve","key":"a","label":"approve","done":"approved"},{"id":"request_changes","key":"r","label":"request changes","done":"changes requested"}]}"##;

pub(super) fn described(app: &mut App, answer: &str) {
    let answer = plugin::json::parse(answer.as_bytes()).expect("JSON");
    app.set_naming(Some(Naming::decode(&answer).expect("decodes")));
}

pub(super) fn session(dir: &tempfile::TempDir, head: &str, last: Option<&str>) -> Session {
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

pub(super) fn said(app: &mut App, id: u64, said: Said) -> Option<Action> {
    app.event(Event::Review { id, said })
}

pub(super) fn focused(app: &App) -> Option<&str> {
    app.doc.focused().map(|t| t.root.as_str())
}

/// [`commented`] with `json` for its comments, in a review session so
/// your comments are drafts, every file placed, `width` × `height`.
pub(super) fn reviewed(
    dir: &tempfile::TempDir,
    json: &str,
    (width, height): (usize, usize),
) -> App {
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
pub(super) fn listed(dir: &tempfile::TempDir, json: &str, size: (usize, usize)) -> App {
    let mut app = reviewed(dir, json, size);
    key(&mut app, 'i');
    app
}

/// The selected thread's root in the comments panel.
pub(super) fn selected(app: &App) -> String {
    let panel = app.panel.as_ref().expect("the comments show");
    panel.selected().unwrap_or_default().to_owned()
}

/// The file shown and the root of the thread focused in it.
pub(super) fn at(app: &App) -> (usize, String) {
    let focused = app.doc.focused().map(|t| t.root.clone());
    (app.doc.document.pos.file, focused.unwrap_or_default())
}

/// `json` with general comments, on the review rather than a file, as
/// a review plugin imports them: the description last in the file and
/// the others out of date order.
pub(super) fn with_conversation(json: &str) -> String {
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
pub(super) fn conversing(
    dir: &tempfile::TempDir,
    (width, height): (usize, usize),
) -> (App, PathBuf) {
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

/// The cell where `text` first shows on screen under the title row,
/// which names the file shown.
pub(super) fn cell_of(app: &mut App, text: &str) -> (u16, u16) {
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

pub(super) fn right_click(app: &mut App, text: &str) {
    let (x, y) = cell_of(app, text);
    app.event(Event::Input(Input::RightClick { x, y }));
}

/// The open menu's entries, each its label and its key.
pub(super) fn menu_rows(app: &App) -> Vec<String> {
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
pub(super) fn choose(app: &mut App, label: &str) -> Option<Action> {
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

pub(super) fn choose_again(app: &mut App, text: &str, label: &str) -> Option<Action> {
    right_click(app, text);
    choose(app, label)
}

use super::support::*;
use crate::control::viewer::{Command, Inbound, Reply};

/// `view/open` of `path` at `line` of the newer version.
fn view_open(app: &mut App, path: &str, line: usize) {
    view_open_at(app, path, line, document::Side::New);
}

/// `view/open` of `path` at `line` of `side`.
fn view_open_at(app: &mut App, path: &str, line: usize, side: document::Side) {
    let (reply, _answer) = Reply::channel();
    let command = Command::Open {
        path: path.to_owned(),
        line: Some(line),
        side,
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

/// In file view, a hunk opened in place shows the removed line beside the
/// new one it was edited into: a jump to either marks its own row.
#[test]
fn a_jump_into_a_hunk_opened_in_file_view_marks_its_row() {
    let mut app = app(100, 24);
    key(&mut app, 'l');
    key(&mut app, 'G');
    let (x, y) = cell_of(&mut app, "line fifty");
    app.event(Event::Input(Input::Click { x, y }));
    key(&mut app, 'x');
    let shown = screen(&mut app);
    assert!(
        shown.contains("line 50") && shown.contains("line fifty"),
        "the hunk opened in place: {shown}"
    );
    view_open_at(&mut app, FILE, 49, document::Side::Old);
    let rows = landed_rows(&mut app);
    assert!(
        rows.len() == 1 && rows[0].1.contains("line 50") && rows[0].1.contains('❯'),
        "{rows:?}"
    );
    view_open(&mut app, FILE, 49);
    let rows = landed_rows(&mut app);
    assert!(
        rows.len() == 1 && rows[0].1.contains("line fifty") && rows[0].1.contains('❯'),
        "{rows:?}"
    );
    assert!(screen(&mut app).contains("line 50"), "the hunk stayed open");
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

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs")
        .success();
    assert!(ok, "git {args:?}");
}

/// A viewer of a repository's uncommitted changes to `README.md`,
/// `mm/use.c` and `mm/x.c`, nine lines each, the third changed, kept
/// current as the watcher's refreshes are.
struct Watched {
    dir: tempfile::TempDir,
    repo: Repo,
    set: ChangeSet,
    app: App,
}

impl Watched {
    const FILES: [&str; 3] = ["README.md", "mm/use.c", "mm/x.c"];

    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = dir.path();
        git(d, &["init", "-q", "-b", "main"]);
        std::fs::create_dir(d.join("mm")).expect("mkdir");
        for path in Self::FILES {
            std::fs::write(d.join(path), Self::text(path, false)).expect("write");
        }
        std::fs::write(d.join("a.txt"), Self::text("a.txt", false)).expect("write");
        std::fs::write(d.join("b.txt"), Self::text("b.txt", false)).expect("write");
        git(d, &["add", "-A"]);
        git(d, &["commit", "-q", "-m", "first"]);
        for path in Self::FILES {
            std::fs::write(d.join(path), Self::text(path, true)).expect("write");
        }
        let repo = Repo::discover(d).expect("repo");
        let spec = repo.resolve(&[], false, false).expect("resolves");
        let mut set = ChangeSet::new(repo.clone(), spec, Revisions::default(), &[], false);
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "uncommitted".to_owned(),
            None,
            (100, 24),
            Indicators::Classic,
        );
        set.enumerate(|change| {
            app.event(Event::Enumerated(change));
            ControlFlow::Continue(())
        })
        .expect("enumerates");
        app.event(Event::EnumerationDone(None));
        app.set_snapshots(spec.snapshots(false));
        let mut watched = Self {
            dir,
            repo,
            set,
            app,
        };
        watched.load();
        watched
    }

    fn text(path: &str, changed: bool) -> String {
        (1..=9)
            .map(|i| {
                let new = if changed && i == 3 { " changed" } else { "" };
                format!("{path} row {i}{new}\n")
            })
            .collect::<Vec<_>>()
            .concat()
    }

    fn write(&self, path: &str, text: &str) {
        std::fs::write(self.dir.path().join(path), text).expect("write");
    }

    /// Reads the list again for `path`, as its file changed on disk.
    fn refresh(&mut self, path: &str) {
        let update = self.set.refresh(&[path.into()]).expect("refreshes");
        self.app.event(Event::Refreshed(update.expect("a change")));
    }

    /// Diffs the files that want it, as the pool does.
    fn load(&mut self) {
        loop {
            self.app.draw();
            let diffs: Vec<_> = (self.app.jobs().into_iter())
                .filter_map(|job| match job {
                    Job::Diff { file, change, .. } => Some((file, change)),
                    _ => None,
                })
                .collect();
            if diffs.is_empty() {
                return;
            }
            for (file, change) in diffs {
                let fd = (self.repo)
                    .file_diff(change, &Options::default())
                    .expect("diffs");
                let done = diffed(&self.app, file, fd);
                self.app.event(Event::Diffed(done));
            }
        }
    }

    fn marked(&mut self) -> Vec<String> {
        // Some of the changes are made on the document alone.
        self.app.dirty = true;
        (landed_rows(&mut self.app).into_iter())
            .map(|(_, row)| row)
            .collect()
    }

    #[track_caller]
    fn assert_marked(&mut self, text: &str) {
        let rows = self.marked();
        assert!(
            rows.len() == 1 && rows[0].contains(text) && rows[0].contains('❯'),
            "{text}: {rows:?}\n{}",
            screen(&mut self.app)
        );
    }
}

/// A file gone before the marked one in the list moves the mark with its
/// file, before and after the diffs are redone.
#[test]
fn the_mark_stays_as_another_file_leaves_the_list() {
    let mut w = Watched::new();
    view_open(&mut w.app, "mm/x.c", 4);
    w.assert_marked("mm/x.c row 5");
    w.write("README.md", &Watched::text("README.md", false));
    w.refresh("README.md");
    w.assert_marked("mm/x.c row 5");
    w.load();
    w.assert_marked("mm/x.c row 5");
}

/// The mark goes with its file: another that takes its place in the list
/// isn't marked, nor is the file once it's listed again.
#[test]
fn the_mark_goes_as_its_file_leaves_the_list() {
    let mut w = Watched::new();
    view_open(&mut w.app, "mm/use.c", 4);
    w.assert_marked("mm/use.c row 5");
    w.write("mm/use.c", &Watched::text("mm/use.c", false));
    w.refresh("mm/use.c");
    assert_eq!(w.marked(), [""; 0]);
    w.write("mm/use.c", &Watched::text("mm/use.c", true));
    w.refresh("mm/use.c");
    w.app.doc.goto_file(1);
    w.app.dirty = true;
    w.load();
    assert_eq!(w.marked(), [""; 0]);
    assert!(screen(&mut w.app).contains("mm/use.c row 5"));
}

/// Files read again as they were, as a rescan has them, keep the mark;
/// edited, its version's lines are others. The older version's stay.
#[test]
fn the_mark_goes_once_the_text_of_its_version_changes() {
    let mut w = Watched::new();
    view_open(&mut w.app, "mm/use.c", 4);
    git(w.dir.path(), &["add", "README.md"]);
    let update = w.set.rescan().expect("rescans").expect("a change");
    w.app.event(Event::Refreshed(update));
    w.load();
    w.assert_marked("mm/use.c row 5");
    let edited = format!("first\n{}", Watched::text("mm/use.c", true));
    w.write("mm/use.c", &edited);
    w.refresh("mm/use.c");
    w.load();
    assert!(screen(&mut w.app).contains("mm/use.c row 5"));
    assert_eq!(w.marked(), [""; 0]);

    view_open_at(&mut w.app, "mm/use.c", 4, document::Side::Old);
    w.assert_marked("mm/use.c row 5");
    w.write("mm/use.c", &format!("another\n{edited}"));
    w.refresh("mm/use.c");
    w.load();
    w.assert_marked("mm/use.c row 5");
}

/// The conversation's page coming first, and going, moves the mark with
/// its file.
#[test]
fn the_mark_stays_as_the_conversation_comes_and_goes() {
    let mut w = Watched::new();
    view_open(&mut w.app, "mm/use.c", 4);
    w.app.doc.set_page(Some(1));
    w.assert_marked("mm/use.c row 5");
    w.app.doc.set_page(None);
    w.assert_marked("mm/use.c row 5");
}

/// The list put in path order, once it's read, moves the mark with its
/// file.
#[test]
fn the_mark_stays_as_the_list_is_sorted() {
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(diff("b.rs").change),
        (100, 24),
        Indicators::Classic,
    );
    app.event(Event::Enumerated(diff("a.rs").change));
    let done = diffed(&app, 0, diff("b.rs"));
    app.event(Event::Diffed(done));
    view_open(&mut app, "b.rs", 4);
    assert_eq!(landed_rows(&mut app).len(), 1);
    app.event(Event::EnumerationDone(None));
    assert_eq!(app.doc.document.pos.file, 1, "sorted");
    let rows = landed_rows(&mut app);
    assert!(
        rows.len() == 1 && rows[0].1.contains("line 5") && rows[0].1.contains('❯'),
        "{rows:?}"
    );
}

/// A file opened outside the diff closing moves the mark with the one
/// after it; the one marked read again at another content loses it.
#[test]
fn the_mark_follows_a_file_opened_outside_the_diff() {
    let on_disk = |id| Side::Worktree {
        mode: Mode::File,
        id,
    };
    let mut w = Watched::new();
    for path in ["a.txt", "b.txt"] {
        let found = Ok(Some(on_disk(None)));
        (w.app).other_found(path.into(), document::Side::New, found);
        w.load();
    }
    w.app.doc.goto_line(4, document::Side::New, 4);
    w.assert_marked("b.txt row 5");
    let gone = vec![("a.txt".into(), document::Side::New, Ok(None))];
    w.app.relooked(gone);
    assert_eq!(w.app.doc.document.pos.file, 3);
    w.assert_marked("b.txt row 5");

    w.write(
        "b.txt",
        &format!("first\n{}", Watched::text("b.txt", false)),
    );
    let hex = "ab".repeat(20);
    let id = dffy_core::git::ObjectId::from_hex(hex.as_bytes()).expect("hex");
    let moved = Ok(Some(on_disk(Some(id))));
    (w.app).relooked(vec![("b.txt".into(), document::Side::New, moved)]);
    w.load();
    assert!(screen(&mut w.app).contains("b.txt row 5"));
    assert_eq!(w.marked(), [""; 0]);
}

/// A viewer of a Markdown file of five paragraphs, the second edited,
/// with a renderer, whose rendering of it has three lines a paragraph.
fn markdown() -> (App, Done) {
    let side = Some(Side::Worktree {
        mode: Mode::File,
        id: None,
    });
    let source = |edited: &str| {
        Text::new(format!("one\n\n{edited}\n\nthree\n\nfour\n\nfive\n").into_bytes())
    };
    let (old, new) = (source("two"), source("second"));
    let fd = FileDiff {
        change: FileChange {
            path: "README.md".into(),
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
    };
    let mut config = Config::default();
    config.set("render", "md:fake").expect("valid");
    let mut app = app_of(&config, "working tree", fd, (120, 24));
    // Renderings drawn for another width than the first frame's are let go.
    screen(&mut app);
    let blocks = |edited: &str| plugin::Side {
        blocks: (["one", edited, "three", "four", "five"].iter().enumerate())
            .map(|(i, text)| plugin::Block {
                kind: "paragraph".to_owned(),
                source: 2 * i..2 * i + 1,
                lines: [format!("{text} drawn"), format!("{text} on"), String::new()]
                    .map(|text| plugin::Line {
                        text,
                        paint: Vec::new(),
                        links: Vec::new(),
                    })
                    .to_vec(),
                language: None,
            })
            .collect(),
    };
    let diff = Arc::clone(app.doc.document.diff_of(0).expect("diffed"));
    let sides = [blocks("two"), blocks("second")];
    let rendered = view::rendered::build(Arc::clone(&diff), sides, crate::grammars(), 3, 60);
    let done = Done::Rendered {
        file: 0,
        version: app.version(0),
        id: app.render_id,
        diff,
        result: Ok(Box::new(rendered)),
    };
    (app, done)
}

/// The rendered view has lines of its own: the mark goes as it's shown.
#[test]
fn the_mark_goes_as_the_rendered_view_is_shown() {
    let (mut app, done) = markdown();
    app.event(Event::Diffed(done));
    view_open(&mut app, "README.md", 4);
    let rows = landed_rows(&mut app);
    assert!(rows.len() == 1 && rows[0].1.contains("three"), "{rows:?}");
    app.doc.set_rendered_mode(true);
    app.dirty = true;
    assert!(screen(&mut app).contains("second drawn"));
    assert_eq!(landed_rows(&mut app), []);
}

/// So it does as a rendering asked for arrives after the jump.
#[test]
fn the_mark_goes_as_a_rendering_arrives() {
    let (mut app, done) = markdown();
    app.doc.set_rendered_mode(true);
    view_open(&mut app, "README.md", 4);
    let rows = landed_rows(&mut app);
    assert!(rows.len() == 1 && rows[0].1.contains("three"), "{rows:?}");
    app.event(Event::Diffed(done));
    assert!(screen(&mut app).contains("second drawn"));
    assert_eq!(landed_rows(&mut app), []);
}

/// And as `r` puts the line diff back, which moves nothing on a file that
/// fits the screen.
#[test]
fn the_mark_goes_as_the_line_diff_comes_back() {
    let (mut app, done) = markdown();
    app.doc.set_rendered_mode(true);
    app.event(Event::Diffed(done));
    view_open(&mut app, "README.md", 4);
    let rows = landed_rows(&mut app);
    assert!(
        rows.len() == 1 && rows[0].1.contains("second on"),
        "{rows:?}"
    );
    key(&mut app, 'r');
    assert!(!screen(&mut app).contains("second on"));
    assert_eq!(landed_rows(&mut app), []);
}

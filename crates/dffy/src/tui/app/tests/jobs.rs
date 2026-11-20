use super::support::*;

/// A new file of `lines` lines: one whole-file hunk.
fn added(lines: usize) -> FileDiff {
    let new = (1..=lines)
        .map(|i| format!("line {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    let (old, new) = (Text::new(Vec::new()), Text::new(new.into()));
    FileDiff {
        change: FileChange {
            path: "notes.txt".into(),
            status: Status::Added,
            old: None,
            new: Some(Side::Worktree {
                mode: Mode::File,
                id: None,
            }),
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

/// The scope line is a job of its own, asked for once the file's
/// highlighting is ready: until it's done the title keeps the hunk's
/// heading, then it shows the definitions.
#[test]
fn the_scope_line_fills_in_when_its_job_is_done() {
    let body = (1..=30)
        .map(|i| format!("        let x{i} = {i};\n"))
        .collect::<Vec<_>>()
        .concat();
    let old = format!("impl Thing {{\n    fn run(&self) {{\n{body}    }}\n}}\n");
    let new = old.replace("let x15 = 15;", "let x15 = 16;");
    let side = Some(Side::Worktree {
        mode: Mode::File,
        id: None,
    });
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    let fd = FileDiff {
        change: FileChange {
            path: "src/thing.rs".into(),
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
    let prepare = File::prepare(crate::grammars(), None);
    let loaded = prepare.file(fd, Whitespace::Exact, true);
    let mut app = app_with(&Config::default(), "working tree", loaded, (100, 20));
    screen(&mut app);
    let Some(Job::Emphasis {
        file,
        version,
        hunk,
        diff,
        syntax,
    }) = app
        .jobs()
        .into_iter()
        .find(|j| matches!(j, Job::Emphasis { .. }))
    else {
        panic!("emphasis for the hunk shown");
    };
    let (emphasis, spans) = prepare.emphasis(&diff, hunk, syntax.as_ref());
    app.event(Event::Diffed(Done::Emphasis {
        file,
        version,
        diff,
        hunk,
        emphasis,
        syntax: spans,
    }));
    let scope = "impl Thing › fn run";
    assert!(!screen(&mut app).contains(scope));
    let Some(Job::Scopes {
        file,
        version,
        diff,
        syntax,
    }) = app
        .jobs()
        .into_iter()
        .find(|j| matches!(j, Job::Scopes { .. }))
    else {
        panic!("the scope job, once the highlighting is ready");
    };
    prepare.scopes(&diff, &syntax);
    app.event(Event::Diffed(Done::Scopes { file, version }));
    assert!(screen(&mut app).contains(scope), "{}", screen(&mut app));
    assert!(!app.jobs().iter().any(|j| matches!(j, Job::Scopes { .. })));
}

/// Each language in the list goes to the pool to compile ahead once, as
/// its first file is listed. With highlighting off none do, until it's
/// turned on.
#[test]
fn each_listed_language_is_compiled_ahead_once() {
    let names = |languages: Vec<Language>| -> Vec<String> {
        languages.iter().map(|l| l.name().to_owned()).collect()
    };
    let viewer = |config: &Config, first: Option<FileChange>| {
        App::new(
            config,
            Theme::default_theme(),
            "working tree".to_owned(),
            first,
            (100, 20),
            Indicators::Classic,
        )
    };
    let mut app = viewer(&Config::default(), Some(diff("src/main.rs").change));
    for path in [
        "src/lib.rs",
        "App.swift",
        "README",
        "Cargo.lock",
        "Sources/B.swift",
    ] {
        app.event(Event::Enumerated(diff(path).change));
    }
    assert_eq!(names(app.take_languages()), ["rust", "swift", "toml"]);
    app.event(Event::Enumerated(diff("src/c.rs").change));
    app.event(Event::Enumerated(diff("b.kt").change));
    app.event(Event::EnumerationDone(None));
    assert_eq!(names(app.take_languages()), ["kotlin"]);
    assert_eq!(names(app.take_languages()), [""; 0]);

    let off = Config {
        highlight: false,
        ..Config::default()
    };
    let mut app = viewer(&off, None);
    app.event(Event::Enumerated(diff("src/lib.rs").change));
    assert_eq!(names(app.take_languages()), [""; 0]);
    app.rediff(Act::ToggleHighlight);
    assert_eq!(names(app.take_languages()), ["rust"]);
}

/// The file is diffed again while emphasis for the diff still shown is
/// being computed, and the new diff lands first: that emphasis was
/// computed for the old hunk and must not be drawn with the new one.
/// The pool held back the new hunk's job while the old one ran, so
/// dropping the old result draws again to ask for it.
#[test]
fn emphasis_for_a_diff_since_replaced_is_dropped() {
    let mut app = app_of(&Config::default(), "working tree", added(46), (187, 80));
    screen(&mut app);
    // Diffing again: the old diff stays shown until the new one lands.
    key(&mut app, ' ');
    key(&mut app, 'I');
    // The panel stays open after a toggle, over the rows looked at.
    press(&mut app, Key::Esc);
    screen(&mut app);
    let jobs = app.jobs();
    let Some(Job::Emphasis {
        file,
        version,
        hunk,
        diff,
        ..
    }) = jobs.into_iter().find(|j| matches!(j, Job::Emphasis { .. }))
    else {
        panic!("emphasis for the hunk shown");
    };
    assert_eq!(diff.new.len(), 46);
    app.event(Event::Diffed(Done::Diff {
        placed: None,
        file,
        version,
        result: Ok(Box::new(loaded(added(60)))),
    }));
    assert!(screen(&mut app).contains("line 60"));
    app.event(Event::Diffed(Done::Emphasis {
        file,
        version,
        hunk,
        emphasis: dffy_core::diff::emphasize(&diff.old, &diff.new, &diff.hunks[hunk]),
        syntax: None,
        diff,
    }));
    assert!(app.draw().is_some());
    let current = app.doc.document.diff_of(file).map(Arc::clone);
    assert!(app.jobs().iter().any(|j| matches!(
        j,
        Job::Emphasis { diff, .. } if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, diff))
    )));
}

/// A watch refresh voids the work of the files it touched and no
/// other: under constant writes to one file, another's emphasis still
/// lands, and a diff of the written file taken before the write is
/// never shown as current.
#[test]
fn a_refresh_of_one_file_keeps_the_work_for_the_others() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(d)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "a@example.com")
            .env("GIT_COMMITTER_NAME", "A")
            .env("GIT_COMMITTER_EMAIL", "a@example.com")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(d.join("a.txt"), "a\n").expect("write");
    std::fs::write(d.join("b.txt"), "one\ntwo\n").expect("write");
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "x"]);
    std::fs::write(d.join("a.txt"), "a2\n").expect("write");
    std::fs::write(d.join("b.txt"), "one\nTWO\n").expect("write");

    let repo = Repo::discover(d).expect("repo");
    let spec = repo.resolve(&[], false, false).expect("spec");
    let mut set = ChangeSet::new(repo.clone(), spec, Revisions::default(), &[], false);
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        None,
        (100, 20),
        Indicators::Classic,
    );
    set.enumerate(|change| {
        app.event(Event::Enumerated(change));
        ControlFlow::Continue(())
    })
    .expect("enumerate");
    app.event(Event::EnumerationDone(None));
    let diffed = |app: &App, file: usize| {
        let change = app.doc.document.files()[file].change().clone();
        let fd = repo.file_diff(change, &Options::default()).expect("diff");
        diffed(app, file, fd)
    };
    for file in [0, 1] {
        let done = diffed(&app, file);
        app.event(Event::Diffed(done));
    }
    key(&mut app, 'f');
    assert_eq!(
        app.doc.document.files()[app.doc.document.pos.file]
            .change()
            .path,
        "b.txt"
    );
    app.draw();
    let emphasis = app
        .jobs()
        .into_iter()
        .find_map(|job| match job {
            Job::Emphasis {
                file,
                version,
                hunk,
                diff,
                ..
            } => Some(Done::Emphasis {
                file,
                version,
                hunk,
                emphasis: dffy_core::diff::emphasize(&diff.old, &diff.new, &diff.hunks[hunk]),
                syntax: None,
                diff,
            }),
            _ => None,
        })
        .expect("emphasis for b.txt's hunk");
    let before_the_write = diffed(&app, 0);

    std::fs::write(d.join("a.txt"), "a3\n").expect("write");
    let update = set
        .refresh(&["a.txt".into()])
        .expect("refresh")
        .expect("changed");
    app.event(Event::Refreshed(update));
    app.event(Event::Diffed(emphasis));
    app.event(Event::Diffed(before_the_write));

    assert!(app.draw().is_some());
    assert!(
        !app.jobs().iter().any(|j| matches!(j, Job::Emphasis { .. })),
        "b.txt's emphasis was kept"
    );
    assert!(
        app.doc.document.files()[0].wants_diff(),
        "a.txt's diff from before the write isn't taken"
    );
}

/// `n` into a file not diffed yet waits at its top; `n` again waits
/// too, rather than skipping the file's changes; the diff arriving
/// opens the file as it opens the first time, its first change in view
/// (on the first screen here, so from the top), and the next `n` goes
/// on past that change.
#[test]
fn n_into_a_file_not_diffed_yet_waits_for_its_diff() {
    let mut app = app(100, 16);
    for _ in 0..3 {
        key(&mut app, 'n');
    }
    assert_eq!(
        (app.doc.document.pos.file, app.doc.document.pos.part),
        (1, 0)
    );
    key(&mut app, 'n');
    assert_eq!(
        (app.doc.document.pos.file, app.doc.document.pos.part),
        (1, 0)
    );
    assert!(screen(&mut app).contains("waiting for this file's diff"));
    let loaded = loaded(diff("src/zz.rs"));
    app.event(Event::Diffed(Done::Diff {
        placed: None,
        file: 1,
        version: app.version(1),
        result: Ok(Box::new(loaded)),
    }));
    assert_eq!(app.doc.document.pos.file, 1);
    assert!(app.doc.document.pos.part > 0, "{:?}", app.doc.document.pos);
    let shown = screen(&mut app);
    let top = shown.lines().nth(1).unwrap_or_default();
    assert!(top.contains("@@ -1,7"), "{shown}");
    assert!(shown.contains("-│ line 4"), "{shown}");
    // The second change is near the end: the view goes as far down as
    // the file goes.
    key(&mut app, 'n');
    let shown = screen(&mut app);
    let top = shown.lines().nth(1).unwrap_or_default();
    assert!(top.contains("+│ line four"), "{shown}");
    assert!(shown.contains("-│ line 50"), "{shown}");
}

/// With `context = full`, `f` opens a file whose first change is at
/// line 400 a few rows above that change, whether its diff came before
/// or after; a tap's release meanwhile changes nothing.
#[test]
fn a_full_file_opens_at_its_first_change() {
    let late = || {
        let old = (1..=500)
            .map(|i| format!("line {i}\n"))
            .collect::<Vec<_>>()
            .concat();
        let new = old.replace("line 400\n", "line four hundred\n");
        FileDiff {
            hunks: hunks(
                &Text::new(old.clone().into()),
                &Text::new(new.clone().into()),
                &Options::default(),
            ),
            old: Text::new(old.into()),
            new: Text::new(new.into()),
            ..diff("src/zz.rs")
        }
    };
    let mut config = Config::default();
    config.apply_source("context = full\n", &mut Vec::new());
    for diff_first in [true, false] {
        let mut app = app_of(&config, "working tree", diff("src/a.rs"), (100, 16));
        let deliver = |app: &mut App| {
            app.event(Event::Diffed(Done::Diff {
                placed: None,
                file: 1,
                version: app.version(1),
                result: Ok(Box::new(loaded(late()))),
            }));
        };
        if diff_first {
            deliver(&mut app);
            key(&mut app, 'f');
        } else {
            key(&mut app, 'f');
            app.event(Event::Input(Input::Release { x: 50, y: 5 }));
            deliver(&mut app);
        }
        let shown = screen(&mut app);
        let first = shown.lines().nth(1).unwrap_or_default();
        assert!(first.contains("397 397"), "{shown}");
        assert!(shown.contains("-│ line 400"), "{shown}");
    }
}

/// `dffy show`: the commit's files are counted in the background, and
/// its header gives their total only once every one is counted, never
/// the zeros or partial sum it has before.
#[test]
fn a_commit_header_waits_for_every_files_counts() {
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "show HEAD".to_owned(),
        None,
        (100, 20),
        Indicators::Classic,
    );
    app.event(Event::Piped(piped::Item::Commit(Arc::new(
        dffy_core::patch::Commit {
            id: Some("0123abc".repeat(5) + "01234"),
            subject: "Change two files".to_owned(),
            lines: vec![(
                dffy_core::patch::TextKind::CommitMessage,
                "    Change two files".into(),
            )],
        },
    ))));
    for path in ["a.rs", "b.rs"] {
        app.event(Event::Enumerated(diff(path).change));
    }
    app.event(Event::EnumerationDone(None));
    let shows = |app: &mut App, summary: &str| {
        let shown = screen(app);
        assert!(shown.contains(summary), "{shown}");
    };
    shows(&mut app, "2 files …");
    let count = |added, removed| Counts { added, removed };
    app.event(Event::Diffed(Done::Counts {
        counts: vec![(1, app.version(1), count(3, 1))],
    }));
    shows(&mut app, "2 files …");
    app.event(Event::Diffed(Done::Counts {
        counts: vec![(2, app.version(2), count(2, 0))],
    }));
    shows(&mut app, "2 files +5 -1");
}

/// The count pass starts with the files on the tree's screen, then a
/// screen's worth below and above them, and starts again with what's on
/// screen when the tree scrolls to files not counted yet.
#[test]
fn the_count_pass_starts_where_the_tree_shows() {
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "range".to_owned(),
        None,
        (200, 12),
        Indicators::Classic,
    );
    for i in 0..80 {
        app.event(Event::Enumerated(diff(&format!("f{i:02}")).change));
    }
    app.event(Event::EnumerationDone(None));
    screen(&mut app);
    app.tree.scroll_by(20);
    screen(&mut app);
    let shown: Vec<usize> = app.tree.files_on_screen().collect();
    let (top, height) = (shown[0], shown.len());
    let pass = |app: &mut App| -> Option<Vec<usize>> {
        Some(app.take_count_pass()?.iter().map(|f| f.0).collect())
    };
    let first = pass(&mut app).expect("a pass once the list is complete");
    let near: Vec<usize> = (top..top + 2 * height).chain(top - height..top).collect();
    assert_eq!(first[..near.len()], near[..]);
    assert_eq!(first.len(), 80);
    assert_eq!(pass(&mut app), None, "started again unasked");

    let counts = near
        .iter()
        .map(|&f| (f, app.version(f), Counts::default()))
        .collect();
    app.event(Event::Diffed(Done::Counts { counts }));
    app.tree.scroll_by(height.cast_signed());
    screen(&mut app);
    assert_eq!(pass(&mut app), None, "scrolled to files already counted");
    app.tree.scroll_by((2 * height).cast_signed());
    screen(&mut app);
    let shown: Vec<usize> = app.tree.files_on_screen().collect();
    let again = pass(&mut app).expect("scrolled to files not counted");
    assert_eq!(again[..shown.len()], shown[..]);
    assert_eq!(again.len(), 80 - near.len());
}

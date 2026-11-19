use super::support::*;

/// A modified file whose new side is `text`.
fn searchable(path: &str, text: &str) -> FileDiff {
    let old = Text::new(b"one\ntwo\n".to_vec());
    let new = Text::new(text.as_bytes().to_vec());
    let hunks = hunks(&old, &new, &Options::default());
    let side = Some(Side::Worktree {
        mode: Mode::File,
        id: None,
    });
    FileDiff {
        change: FileChange {
            path: path.into(),
            status: Status::Modified,
            old: side,
            new: side,
            staging: None,
        },
        old,
        new,
        binary: false,
        too_large: false,
        note: None,
        hunks,
    }
}

/// Two files to search: `src/a.rs` with `needle` twice, `src/b.rs`
/// with `Needle` once, both diffed.
fn search_app() -> App {
    let a = searchable("src/a.rs", "one\nneedle here\nand a needle\n");
    let b = searchable("src/b.rs", "Needle\ntwo\n");
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(a.change.clone()),
        (100, 20),
        Indicators::Classic,
    );
    app.event(Event::Enumerated(b.change.clone()));
    app.event(Event::EnumerationDone(None));
    for (i, fd) in [a, b].into_iter().enumerate() {
        let version = app.version(i);
        app.event(Event::Diffed(Done::Diff {
            placed: None,
            file: i,
            version,
            result: Ok(Box::new(loaded(fd))),
        }));
    }
    app
}

/// The search's jobs, done as the workers would: the matches in the
/// file shown, and the counts of the others.
fn work(app: &mut App) {
    for job in app.jobs() {
        let done = match job {
            Job::Find {
                file,
                version,
                diff,
                hidden,
                blocks,
                blocks_version,
                needle,
                id,
                ..
            } => Done::Found {
                file,
                version,
                id,
                matches: crate::tui::worker::find(
                    diff.as_deref(),
                    &hidden,
                    &blocks,
                    &needle,
                    || false,
                ),
                diff,
                hidden,
                blocks_version,
            },
            Job::Search {
                file,
                version,
                needle,
                id,
                ..
            } => Done::Search {
                file,
                version,
                id,
                count: Some(needle.find_all(b"Needle\n").len()),
            },
            _ => continue,
        };
        app.event(Event::Diffed(done));
    }
}

/// `/` searches as it's typed, the workers marking matches on screen
/// as they find them; `enter` goes to the first; `n`/`N` go between
/// matches and on into the next file; the status bar counts; `esc`
/// gives `n` back to changes.
#[test]
fn the_diff_search_marks_jumps_and_counts() {
    let mut app = search_app();
    app.draw();
    for c in "/needle".chars() {
        key(&mut app, c);
    }
    assert!(
        app.doc.document.search.is_none(),
        "typing waits for no scan"
    );
    assert!(screen(&mut app).contains("… in this file"));
    work(&mut app);
    let marks = app
        .doc
        .document
        .search
        .as_ref()
        .map(|m| m.matches.list.len());
    assert_eq!(marks, Some(2), "marked once found");
    app.draw();
    let lit = app.theme.style(Role::SearchMatch);
    let cells = (0..20)
        .flat_map(|y| app.screen.shown_grid().row(y).iter())
        .filter(|c| c.style() == lit)
        .count();
    assert_eq!(
        cells,
        2 * "needle".len(),
        "both matches drawn in the match style"
    );
    let shown = status(&mut app);
    assert!(shown.contains("/needle▏  2 in this file"), "{shown}");
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    work(&mut app);
    let shown = status(&mut app);
    assert!(shown.contains("match 1/3"), "{shown}");
    key(&mut app, 'n');
    let shown = status(&mut app);
    assert!(shown.contains("match 2/3"), "{shown}");
    // On into src/b.rs, where smart case still finds `Needle`, once
    // its matches arrive.
    key(&mut app, 'n');
    assert_eq!(app.doc.document.pos.file, 1);
    work(&mut app);
    let shown = status(&mut app);
    assert!(shown.contains("match 3/3"), "{shown}");
    key(&mut app, 'N');
    assert_eq!(app.doc.document.pos.file, 0);
    esc(&mut app);
    assert!(app.search.is_none());
    key(&mut app, 'g');
    key(&mut app, 'n');
    assert!(app.search.is_none(), "n goes to changes again");
}

/// `/` finds comments' text with the code's: `enter` goes to the first
/// match, on the line, and `n` to the next, in the box under it; one
/// only in a comment is found too.
#[test]
fn the_diff_search_finds_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    let current = |app: &App| app.doc.document.search.as_ref().and_then(|m| m.current);
    typed(&mut app, "/fifty");
    work(&mut app);
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    assert!(
        status(&mut app).contains("match 1/2"),
        "{}",
        status(&mut app)
    );
    let top = app.doc.document.pos;
    key(&mut app, 'n');
    assert!(
        status(&mut app).contains("match 2/2"),
        "{}",
        status(&mut app)
    );
    assert_eq!(current(&app), Some(1));
    assert!(app.doc.document.pos >= top, "the box is under its line");
    let shown = screen(&mut app);
    let card = shown
        .lines()
        .find(|l| l.contains("Fifty?"))
        .expect("on screen");
    assert!(card.contains('▸'), "{shown}");
    esc(&mut app);
    key(&mut app, 'g');
    typed(&mut app, "/why two");
    work(&mut app);
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    assert_eq!(app.search_status().as_deref(), Some("match 1/1"));
    assert!(screen(&mut app).contains("Why two?"));
}

/// A scan for a cleared search that reports after the next search
/// started is dropped: search ids never repeat.
#[test]
fn a_late_count_from_a_cleared_search_isnt_taken_for_the_next_one() {
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(searchable("a.rs", "f\n").change),
        (100, 20),
        Indicators::Classic,
    );
    app.event(Event::EnumerationDone(None));
    typed(&mut app, "/f");
    let old = app.search.as_ref().map(|s| s.id).expect("searching");
    app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
    typed(&mut app, "/g");
    app.event(Event::Diffed(Done::Search {
        file: 0,
        version: app.version(0),
        id: old,
        count: Some(5),
    }));
    let counts = app.search.as_ref().map(|s| s.counts.clone());
    assert_eq!(counts, Some(vec![None]));
}

/// A search typed while the list is still arriving survives the sort
/// at the end of it: an untracked file listed last moves ahead of the
/// file shown, and the matches stay on the file they were found in.
#[test]
fn search_marks_follow_their_file_when_the_list_is_sorted() {
    let tracked = searchable("src/z.rs", "needle\nneedle\n");
    let untracked = FileDiff {
        change: FileChange {
            status: Status::Untracked,
            old: None,
            ..searchable("src/a.rs", "").change
        },
        ..searchable("src/a.rs", "a needle\n")
    };
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(tracked.change.clone()),
        (100, 20),
        Indicators::Classic,
    );
    app.event(Event::Enumerated(untracked.change.clone()));
    for (file, fd) in [tracked, untracked].into_iter().enumerate() {
        let version = app.version(file);
        app.event(Event::Diffed(Done::Diff {
            placed: None,
            file,
            version,
            result: Ok(Box::new(loaded(fd))),
        }));
    }
    app.draw();
    typed(&mut app, "/needle");
    let stale_jobs = app.jobs();
    work(&mut app);
    assert_eq!(app.doc.document.search.as_ref().map(|m| m.file), Some(0));

    app.event(Event::EnumerationDone(None));
    assert_eq!(app.doc.document.files()[0].change().path, "src/a.rs");
    assert_eq!(app.doc.document.pos.file, 1, "still on src/z.rs");
    let marked = app
        .doc
        .document
        .search
        .as_ref()
        .map(|m| &app.doc.document.files()[m.file].change().path);
    assert!(marked.is_none_or(|path| path == "src/z.rs"), "{marked:?}");
    // A scan started before the sort reports by the old positions.
    for job in stale_jobs {
        if let Job::Find {
            file,
            version,
            diff,
            hidden,
            blocks,
            blocks_version,
            needle,
            id,
            ..
        } = job
        {
            app.event(Event::Diffed(Done::Found {
                file,
                version,
                id,
                matches: crate::tui::worker::find(
                    diff.as_deref(),
                    &hidden,
                    &blocks,
                    &needle,
                    || false,
                ),
                diff,
                hidden,
                blocks_version,
            }));
        }
    }
    work(&mut app);
    let marks = app
        .doc
        .document
        .search
        .as_ref()
        .map(|m| (m.file, m.matches.list.len()));
    assert_eq!(marks, Some((1, 2)), "src/z.rs's two matches");
    app.draw();
    let lit = app.theme.style(Role::SearchMatch);
    let cells = (0..20)
        .flat_map(|y| app.screen.shown_grid().row(y).iter())
        .filter(|c| c.style() == lit)
        .count();
    assert_eq!(cells, 2 * "needle".len());
    let counts = app.search.as_ref().map(|s| s.counts.clone());
    assert_eq!(counts.and_then(|c| c.first().copied()), Some(Some(1)));
}

/// A file whose diff failed has nothing to scan: `enter` and `n` on it
/// go on to the files that have matches instead of waiting.
#[test]
fn a_search_on_a_failed_file_goes_on_to_the_others() {
    let mut app = search_app();
    app.event(Event::Enumerated(diff("src/c.rs").change));
    app.event(Event::Diffed(Done::Diff {
        placed: None,
        file: 2,
        version: app.version(2),
        result: Err("unreadable".to_owned()),
    }));
    key(&mut app, 'f');
    key(&mut app, 'f');
    assert_eq!(app.doc.document.pos.file, 2);
    typed(&mut app, "/needle");
    app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    work(&mut app);
    assert!(!status(&mut app).contains("searching this file"));
    key(&mut app, 'N');
    work(&mut app);
    assert_eq!(
        app.doc.document.pos.file, 1,
        "back to src/b.rs and its match"
    );
    let current = app.doc.document.search.as_ref().and_then(|m| m.current);
    assert_eq!(current, Some(0), "{}", status(&mut app));
}

/// Side by side, a removed line and the added line beside it share a
/// row: `n` takes matches in the order the rows show them, left to
/// right, not in the order of the diff's lines, and `n` into a file
/// lands on the first row's.
#[test]
fn n_goes_through_matches_in_row_order_side_by_side() {
    let split = |old: &[u8], new: &[u8]| {
        let (old, new) = (Text::new(old.to_vec()), Text::new(new.to_vec()));
        let side = Some(Side::Worktree {
            mode: Mode::File,
            id: None,
        });
        let fd = FileDiff {
            change: FileChange {
                path: "src/a.rs".into(),
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
        let mut app = app_of(&Config::default(), "working tree", fd, (160, 20));
        assert!(app.doc.layout.split);
        typed(&mut app, "/needle");
        app
    };
    let current = |app: &App| app.doc.document.search.as_ref().and_then(|m| m.current);
    let mut app = split(b"a needle 1\nb needle 2\n", b"a needle 1!\nb needle 2!\n");
    work(&mut app);
    press(&mut app, Key::Enter);
    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.extend(current(&app));
        key(&mut app, 'n');
    }
    // The diff has both removed lines, then both added ones.
    assert_eq!(seen, [0, 2, 1, 3]);

    // The removed match is the diff's first, on the second row.
    let mut app = split(b"x\nb needle\n", b"a needle\ny\n");
    if let Some(search) = &mut app.search {
        search.pending = Some((0, Land::First));
    }
    work(&mut app);
    assert_eq!(current(&app), Some(1));
}

/// A pattern typed over a running scan stops it: the scan asks, and
/// the search's id has moved on.
#[test]
fn a_newer_pattern_stops_the_scan_of_an_older_one() {
    use crate::tui::worker::find;
    let mut app = search_app();
    typed(&mut app, "/ne");
    let jobs = app.jobs();
    let Some(Job::Find {
        diff,
        hidden,
        live,
        id,
        ..
    }) = jobs.into_iter().find(|j| matches!(j, Job::Find { .. }))
    else {
        panic!("a scan of the file shown");
    };
    key(&mut app, 'e');
    let stale = || live.load(Ordering::Relaxed) != id;
    assert!(stale());
    let needle = Needle::new("ne");
    let diff = diff.as_deref();
    assert_eq!(find(diff, &hidden, &[], &needle, stale), None);
    assert!(find(diff, &hidden, &[], &needle, || false).is_some());
}

/// `/` searches the Conversation's comments: counted from another file,
/// marked once it shows, and `enter` goes to them.
#[test]
fn the_search_finds_the_conversations_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = conversing(&dir, (72, 40));
    app.open(1);
    typed(&mut app, "/ready");
    let counted = app.jobs().into_iter().any(|j| {
        matches!(
            j,
            Job::Search {
                file: 0,
                change: None,
                ..
            }
        )
    });
    assert!(counted, "the page is counted by its comments alone");
    app.open(0);
    app.draw();
    let job = app.jobs().into_iter().find_map(|j| match j {
        Job::Find {
            file: 0,
            diff: None,
            hidden,
            blocks,
            blocks_version,
            needle,
            id,
            version,
            ..
        } => Some(Done::Found {
            file: 0,
            version,
            id,
            matches: document::find_blocks(&blocks, &needle, || false),
            diff: None,
            hidden,
            blocks_version,
        }),
        _ => None,
    });
    app.event(Event::Diffed(job.expect("a scan of the page")));
    let found = app
        .doc
        .document
        .search
        .as_ref()
        .map(|m| (m.file, m.matches.len()));
    assert_eq!(found, Some((0, 1)));
    press(&mut app, Key::Enter);
    assert!(
        status(&mut app).contains("match 1/"),
        "{}",
        status(&mut app)
    );
}

/// A file counted before its comments were placed is counted again
/// once they are: their text has matches too.
#[test]
fn a_file_is_counted_again_once_its_comments_are_placed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    typed(&mut app, "/Moved");
    let id = app.search.as_ref().expect("searching").id;
    let far = 11;
    app.event(Event::Diffed(Done::Search {
        file: far,
        version: app.version(far),
        id,
        count: Some(0),
    }));
    let asked = |app: &App| {
        (app.jobs().iter()).any(|j| matches!(j, Job::Search { file, .. } if *file == far))
    };
    assert!(!asked(&app), "counted");
    let placed = diffed(&app, far, diff("src/zz9.rs"));
    app.event(Event::Diffed(placed));
    assert!(asked(&app), "counted again, with its comments");
}

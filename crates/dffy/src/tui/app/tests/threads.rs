use super::support::*;

/// `c` on a selection opens a text box right under it, where every key
/// types, shift+enter and a paste included; `enter` saves the text on the
/// selected lines with their anchor and distance, and the new thread
/// shows there with its mark in the gutter.
#[test]
fn c_writes_a_comment_in_a_box_under_the_selection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("dffy").join("comments.json");
    let mut app = app(100, 20);
    app.set_comments(path.clone());
    screen(&mut app);
    key(&mut app, 'v');
    key(&mut app, 'j');
    assert_eq!(press(&mut app, Key::Char('c')), None);
    assert!(app.doc.selection.is_none());
    typed(&mut app, "Why these? q");
    press(&mut app, Key::Backspace);
    app.event(Event::Input(Input::Key(Key::Enter, Mods::SHIFT)));
    app.event(Event::Input(Input::Paste("Pasted\r\nlines".to_owned())));
    insta::assert_snapshot!("comment_box", screen(&mut app));
    assert!(!path.exists(), "nothing saved while writing");
    press(&mut app, Key::Enter);
    assert!(app.doc.writing().is_none());
    let saved = read_comments(&path);
    let [comment] = saved.comments.as_slice() else {
        panic!("one comment: {saved:?}");
    };
    assert_eq!(
        (comment.path.as_str(), comment.text.as_str(), comment.side),
        (FILE, "Why these? \nPasted\nlines", review::Side::New)
    );
    assert_eq!(
        (comment.start, comment.end),
        (
            Some(review::LineNumbers {
                old: Some(0),
                new: Some(0)
            }),
            Some(review::LineNumbers {
                old: Some(1),
                new: Some(1)
            })
        )
    );
    let anchor = comment.anchor.as_ref().expect("an anchor");
    assert_eq!(anchor.lines, ["line 1", "line 2"]);
    assert_eq!(anchor.after, ["line 3", "line four"]);
    assert_eq!(comment.distance, Some(2));
    let shown = screen(&mut app);
    // Outside a review session nothing is posted, so nothing's a draft.
    let rows: Vec<&str> = shown.lines().filter(|l| l.contains('●')).collect();
    assert_eq!(rows.len(), 2, "{shown}");
    assert!(rows[0].contains("line 1") && rows[1].contains("line 2"));
    let below = shown
        .lines()
        .skip_while(|l| !l.contains("line 2"))
        .nth(1)
        .unwrap_or_default();
    // Saved with `enter`, the keys stay the diff's.
    assert!(below.contains("▾ you · just now ─"), "{shown}");
    assert_eq!(app.doc.focused(), None);

    // An empty box saves nothing.
    press(&mut app, Key::Esc);
    key(&mut app, 'c');
    typed(&mut app, "  ");
    press(&mut app, Key::Esc);
    assert!(screen(&mut app).contains("empty; nothing saved"));
    assert_eq!(read_comments(&path).comments.len(), 1);

    // Editing a draft down to nothing deletes it.
    key(&mut app, 'm');
    key(&mut app, 'e');
    for _ in 0..200 {
        press(&mut app, Key::Backspace);
    }
    press(&mut app, Key::Esc);
    assert!(screen(&mut app).contains("draft deleted"));
    assert_eq!(read_comments(&path).comments.len(), 0);
}

/// A `d` armed for one draft doesn't outlast the mouse: after a click
/// on another thread, `d` asks again rather than deleting it.
#[test]
fn a_click_disarms_d() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    let shown = screen(&mut app);
    let y = shown
        .lines()
        .position(|l| l.contains("Why two?"))
        .expect("the draft on screen");
    key(&mut app, 'm');
    assert_eq!(focused(&app), Some("f1"));
    key(&mut app, 'd');
    let y = u16::try_from(y).expect("on screen");
    app.event(Event::Input(Input::Click { x: 60, y }));
    app.event(Event::Input(Input::Release { x: 60, y }));
    assert_eq!(focused(&app), Some("d1"));
    key(&mut app, 'd');
    assert_eq!(read_comments(&path).comments.len(), 7, "nothing deleted");
    assert!(status(&mut app).contains("d again deletes"));
}

/// `ctrl+g` hands the box's text to the editor through the compose
/// path; what's written there is saved. When the editor fails, the box
/// opens again with the text. `ctrl+e`, which Ghostty sends for
/// cmd+right, stays in the box.
#[test]
fn ctrl_g_continues_the_box_in_the_editor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = app(100, 20);
    app.set_comments(path.clone());
    screen(&mut app);
    key(&mut app, 'c');
    typed(&mut app, "Draft");
    let ctrl = |c| Event::Input(Input::Key(Key::Char(c), Mods::CTRL));
    assert_eq!(app.event(ctrl('e')), None);
    assert!(app.doc.writing().is_some());
    assert_eq!(
        app.event(ctrl('g')),
        Some(Action::Compose("Draft".to_owned()))
    );
    assert!(app.doc.writing().is_none());
    app.composed(None);
    let reopened = app.doc.writing().map(|w| w.text.text().to_owned());
    assert_eq!(reopened.as_deref(), Some("Draft"));
    assert!(app.event(ctrl('g')).is_some());
    app.composed(Some("Written in vim\n".to_owned()));
    let saved = read_comments(&path);
    assert_eq!(saved.comments.len(), 1);
    assert_eq!(saved.comments[0].text, "Written in vim");
    // The command panel's entry takes the focused thread's draft there,
    // once it's placed.
    screen(&mut app);
    key(&mut app, ' ');
    assert_eq!(
        press(&mut app, Key::Char('E')),
        Some(Action::Compose("Written in vim".to_owned()))
    );
    app.composed(Some("Edited".to_owned()));
    assert_eq!(read_comments(&path).comments[0].text, "Edited");
    // The diff's `E` is the same entry.
    screen(&mut app);
    assert_eq!(
        press(&mut app, Key::Char('E')),
        Some(Action::Compose("Edited".to_owned()))
    );
}

/// When saving the text box fails, it opens again with the text, and a
/// second `esc` discards it rather than trying again.
#[test]
fn a_second_esc_discards_a_box_that_failed_to_save() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("file"), "").expect("write");
    let mut app = app(160, 20);
    app.set_comments(dir.path().join("file/comments.json"));
    screen(&mut app);
    key(&mut app, 'c');
    typed(&mut app, "Kept");
    press(&mut app, Key::Esc);
    assert!(app.doc.writing().is_some(), "open again");
    let shown = status(&mut app);
    assert!(shown.contains("esc again discards"), "{shown}");
    press(&mut app, Key::Esc);
    assert!(app.doc.writing().is_none());
    assert!(status(&mut app).contains("discarded"));
}

/// "Suggest change" left as it opened, the lines as they are and
/// nothing said, saves nothing.
#[test]
fn an_untouched_suggestion_saves_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = app(100, 30);
    app.set_comments(path.clone());
    screen(&mut app);
    key(&mut app, 'S');
    press(&mut app, Key::Esc);
    assert!(!path.exists() || read_comments(&path).comments.is_empty());
}

/// Once applied, a suggestion's thread moves to the lines it put there and
/// stays open, its comment marked applied with what it replaced: drawn
/// so, and `s` takes it back rather than applying it again.
#[test]
fn an_applied_suggestion_moves_its_thread_and_applies_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = app(100, 30);
    app.set_comments(path.clone());
    screen(&mut app);
    key(&mut app, 'S');
    typed(&mut app, "Shorter");
    press(&mut app, Key::Esc);
    screen(&mut app);
    let Some(Action::ApplySuggestion(suggestion)) = press(&mut app, Key::Char('s')) else {
        panic!("not applied: {}", status(&mut app));
    };
    let mut moved = Anchor::new(&suggestion.diff.new, 0..1);
    moved.before = vec!["above".to_owned()];
    app.event(Event::Applied(Ok(annotation::Applied {
        replaced: 0..1,
        lines: 0..1,
        anchor: Some(moved.clone()),
    })));
    let marked = &read_comments(&path).comments[0];
    assert!(marked.applied.is_some(), "{marked:?}");
    assert_eq!(marked.replaced.as_deref(), Some("line 1"));
    assert_eq!(
        (marked.anchor.as_ref(), marked.resolved),
        (Some(&moved), false),
        "on the new lines, still open"
    );
    let shown = screen(&mut app);
    assert!(shown.contains("you · just now · applied ━"), "{shown}");
    assert!(shown.contains("suggested change, applied:"), "{shown}");
    assert!(!shown.contains("s apply"), "{shown}");
    let back = press(&mut app, Key::Char('s'));
    assert!(
        matches!(&back, Some(Action::ApplySuggestion(s)) if s.lines == ["line 1"]),
        "{back:?}"
    );
}

/// `s` on a thread applies its last suggestion to the lines it's on, as
/// the loop writes them; an empty one deletes them, and a thread without
/// one says so. Another `s` while one is applied waits.
#[test]
fn s_on_a_thread_applies_its_suggestion() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = app(100, 30);
    app.set_comments(path.clone());
    screen(&mut app);
    key(&mut app, 'c');
    typed(&mut app, "No suggestion");
    press(&mut app, Key::Esc);
    screen(&mut app);
    assert_eq!(press(&mut app, Key::Char('s')), None);
    assert!(status(&mut app).contains("this thread has no suggestion to apply"));
    esc(&mut app);
    key(&mut app, 'S');
    typed(&mut app, "Shorter");
    press(&mut app, Key::Esc);
    screen(&mut app);
    let root = read_comments(&path).comments[1].id.clone();
    let applied = |app: &mut App| match press(app, Key::Char('s')) {
        Some(Action::ApplySuggestion(suggestion)) => suggestion,
        other => panic!("{other:?}: {}", status(app)),
    };
    let suggestion = applied(&mut app);
    assert_eq!(
        (
            suggestion.at,
            &suggestion.anchor.lines[..],
            &suggestion.lines[..],
            &suggestion.checkout,
            &suggestion.comment,
            &suggestion.diff.change.path
        ),
        (
            0,
            &["line 1".to_owned()][..],
            &["line 1".to_owned()][..],
            &None,
            &root,
            &FILE.into()
        )
    );
    assert_eq!(press(&mut app, Key::Char('s')), None);
    assert!(status(&mut app).contains("still applying"));
    let said = |app: &App| app.message.as_ref().map(|(m, _)| m.clone());
    let done = |replaced: Range<usize>, lines: Range<usize>, anchor: Option<Anchor>| {
        Event::Applied(Ok(annotation::Applied {
            replaced,
            lines,
            anchor,
        }))
    };
    app.event(done(0..1, 0..1, None));
    assert_eq!(
        said(&app),
        Some(format!("applied the suggestion to {FILE}:1 · x resolves"))
    );

    let empty = Op::Add {
        comment: Box::new(review::Comment {
            text: "Drop it".to_owned(),
            suggestion: Some(String::new()),
            author: "octocat".to_owned(),
            ..review::Comment::default()
        }),
        reply_to: Some(Target::Id(root.clone())),
    };
    review::apply(&path, vec![empty]).expect("applies");
    app.event(Event::Comments);
    screen(&mut app);
    assert_eq!(applied(&mut app).lines, Vec::<String>::new());
    app.event(done(0..1, 0..0, None));
    assert_eq!(
        said(&app),
        Some(format!("deleted {FILE}:1, as suggested · x resolves"))
    );

    // Written elsewhere, as through MCP: its line breaks are read as lines,
    // a last one ending the last line rather than adding an empty one.
    for (suggestion, lines) in [
        ("line 1\r\nline 1b\r\n", &["line 1", "line 1b"][..]),
        ("\n", &[""][..]),
    ] {
        let written = Op::Add {
            comment: Box::new(review::Comment {
                text: "This way".to_owned(),
                suggestion: Some(suggestion.to_owned()),
                author: "an-mcp-client".to_owned(),
                ..review::Comment::default()
            }),
            reply_to: Some(Target::Id(root.clone())),
        };
        review::apply(&path, vec![written]).expect("applies");
        app.event(Event::Comments);
        screen(&mut app);
        assert_eq!(applied(&mut app).lines, lines, "{suggestion:?}");
        app.event(done(0..1, 0..lines.len(), None));
    }
}

/// Threads show under the lines they're on, each comment with its
/// author, age and state, replies indented; the comment on the whole
/// file under its first row. The gutter keeps a mark per thread, the
/// tree counts each file's comments, and the status bar counts the
/// threads, and in a review session the drafts.
#[test]
fn threads_show_inline_under_their_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    insta::assert_snapshot!("comment_marks", screen(&mut app));
    app.resize(160, 30);
    let wide = screen(&mut app);
    assert!(wide.contains("6 threads ─"), "{wide}");
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(&mut app, prepare, Said::Done(Ok(())));
    let wide = screen(&mut app);
    assert!(wide.contains("6 threads · 5 drafts"), "{wide}");
}

/// Files with comments beyond the ones on screen and prefetched are
/// diffed so their threads get placed; until then the comments panel
/// says it's still looking, and it lists the far file's threads once
/// they are, those on a file not in the diff included.
#[test]
fn far_files_with_comments_are_diffed_and_every_thread_listed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    screen(&mut app);
    let far = 11;
    assert_eq!(app.doc.document.files()[far].change().path, "src/zz9.rs");
    assert!(
        app.jobs()
            .iter()
            .any(|j| matches!(j, Job::Diff { file, .. } if *file == far)),
        "the far file is diffed"
    );
    key(&mut app, 'i');
    let listed = left(&mut app);
    assert!(listed.contains("finding the lines"), "{listed}");
    assert!(!listed.contains("Moved"), "{listed}");
    let loaded = loaded(diff("src/zz9.rs"));
    app.event(Event::Diffed(Done::Diff {
        placed: None,
        file: far,
        version: app.version(far),
        result: Ok(Box::new(loaded)),
    }));
    assert!(
        !app.jobs()
            .iter()
            .any(|j| matches!(j, Job::Diff { file, .. } if *file == far)),
        "placed, it isn't diffed again"
    );
    let listed = left(&mut app);
    assert!(
        listed.contains("Moved") && listed.contains("Gone"),
        "{listed}"
    );
}

/// The left panel's columns of the screen.
fn left(app: &mut App) -> String {
    let shown = screen(app);
    let width = app.tree_columns().unwrap_or(0);
    (shown.lines())
        .map(|l| l.chars().take(width).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A review session with general comments has a Conversation first in
/// the tree, counted there and not as threads; the files keep their
/// place around it, and it goes with the last general comment. A local
/// review shows none: nothing there comes from a host.
#[test]
fn the_conversation_comes_first_in_a_review_with_general_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    std::fs::write(&path, with_conversation(&review_json("src/zz9.rs"))).expect("write");
    app.event(Event::Comments);
    app.resize(160, 30);
    let local = screen(&mut app);
    assert!(!app.doc.document.files()[0].is_page(), "{local}");
    assert!(
        !local.contains("Conversation") && local.contains("6 threads ─"),
        "{local}"
    );

    app.doc.goto_file(3);
    let on = app.doc.document.files()[3].change().path.clone();
    let built = model::conversed_count();
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(&mut app, prepare, Said::Done(Ok(())));
    let shown = screen(&mut app);
    assert!(app.doc.document.files()[0].is_page());
    let file = app.doc.document.pos.file;
    assert_eq!(
        (file, &app.doc.document.files()[file].change().path),
        (4, &on),
        "on its file still"
    );
    let first = shown.lines().nth(1).unwrap_or_default();
    assert!(
        first.contains("≡ Conversation") && first.contains(" 4 "),
        "{shown}"
    );
    assert!(shown.contains("6 threads · 5 drafts"), "{shown}");
    assert_eq!(
        model::conversed_count(),
        built,
        "nothing made until it shows"
    );

    std::fs::write(&path, review_json("src/zz9.rs")).expect("write");
    app.event(Event::Comments);
    let shown = screen(&mut app);
    assert!(!app.doc.document.files()[0].is_page(), "{shown}");
    let file = app.doc.document.pos.file;
    assert_eq!(
        (file, &app.doc.document.files()[file].change().path),
        (3, &on)
    );
}

/// The Conversation shows the review's title, then its description and
/// the other general comments by date, each in a thread's box with its
/// kind and label. Its comments are made once, however it scrolls, and
/// they're read-only.
#[test]
fn the_conversation_page_shows_the_title_then_each_comment_by_date() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = conversing(&dir, (72, 40));
    let written = std::fs::read(&path).expect("read");
    let built = model::conversed_count();
    app.open(0);
    insta::assert_snapshot!("conversation_page", screen(&mut app));
    assert_eq!(model::conversed_count(), built + 1);
    for _ in 0..6 {
        key(&mut app, 'j');
        screen(&mut app);
    }
    key(&mut app, 'g');
    screen(&mut app);
    assert_eq!(model::conversed_count(), built + 1, "made once");

    key(&mut app, 'm');
    assert_eq!(at(&app), (0, "d1".to_owned()));
    key(&mut app, 'r');
    assert!(status(&mut app).contains("read-only"));
    assert!(app.doc.writing().is_none());
    key(&mut app, ' ');
    assert_eq!(press(&mut app, Key::Char('E')), None, "nor in the editor");
    assert!(app.composing.is_none());
    assert_eq!(std::fs::read(&path).expect("read"), written);
}

/// The comments panel lists the general comments first, apart from the
/// files' threads, which alone its title counts; picking one shows it
/// on the Conversation.
#[test]
fn the_comments_panel_lists_the_conversation_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = conversing(&dir, (130, 30));
    key(&mut app, 'i');
    let listed = left(&mut app);
    let lines: Vec<&str> = listed.lines().collect();
    assert!(
        lines[0].starts_with("┌ Comments · 6 threads · 5 open ─"),
        "{listed}"
    );
    assert!(
        lines[1].contains("Conversation") && lines[1].trim_end().ends_with("4 │"),
        "{listed}"
    );
    let order: Vec<usize> = ["Spells out", "Thanks for", "Is this ready", "Loo"]
        .iter()
        .map(|t| lines.iter().position(|l| l.contains(t)).expect(t))
        .collect();
    assert!(order.is_sorted(), "{listed}");
    assert!(lines[order[3]].contains("review approved"), "{listed}");
    assert!(lines[order[3] + 1].contains(FILE.rsplit('/').next().expect("a name")));
    key(&mut app, 'g');
    assert_eq!(at(&app), (0, "d1".to_owned()));
    key(&mut app, 'c');
    let open = left(&mut app);
    assert!(
        !open.contains("Conversation"),
        "the filters keep threads: {open}"
    );
}

/// `i` turns the left panel to the comment threads, showing it when
/// it's hidden, with the keys, and back to the files; its title says
/// which. Nothing of the list is made while the files show, however
/// the comments change.
#[test]
fn i_switches_the_left_panel_between_files_and_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let json = review_json("src/zz9.rs");
    let mut app = reviewed(&dir, &json, (100, 20));
    key(&mut app, 't');
    assert!(!app.tree_shown);
    let made = review::listing::listed_count();
    key(&mut app, 'i');
    assert!(app.tree_shown && app.pane == Pane::Tree);
    let shown = screen(&mut app);
    assert!(shown.starts_with("┌ Comments · 6 threads ─"), "{shown}");
    assert_eq!(review::listing::listed_count(), made + 1);
    assert!(
        app.take_count_pass().is_none(),
        "no counting for a tree not shown"
    );
    key(&mut app, 'i');
    assert!(app.panel.is_none() && app.pane == Pane::Tree);
    assert!(app.take_count_pass().is_some());
    let shown = screen(&mut app);
    assert!(shown.starts_with("┌ Files · 12 "), "{shown}");
    let path = dir.path().join("comments.json");
    std::fs::write(&path, json.replace("Why two?", "Why 2?")).expect("write");
    app.event(Event::Comments);
    assert!(screen(&mut app).contains("Why 2?"));
    assert_eq!(review::listing::listed_count(), made + 1);
}

/// On a narrow screen the comments open over the diff; `enter` closes
/// them on the thread, and `i` brings them back with the keys.
#[test]
fn narrow_comments_open_over_the_diff_and_come_back_with_i() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = reviewed(&dir, &review_json("src/zz9.rs"), (40, 30));
    key(&mut app, 'i');
    assert!(app.tree_shown && app.pane == Pane::Tree);
    key(&mut app, 'j');
    press(&mut app, Key::Enter);
    assert!(!app.tree_shown);
    assert_eq!((app.pane, at(&app)), (Pane::Diff, (0, "d1".to_owned())));
    key(&mut app, 'i');
    assert!(app.tree_shown && app.pane == Pane::Tree);
    let shown = screen(&mut app);
    assert!(
        shown
            .lines()
            .nth(1)
            .is_some_and(|l| l.starts_with("┌ Comments")),
        "{shown}"
    );
}

/// A thread listed before its file's diff is in, one on the whole
/// file: following it opens the file and waits, as `m` does, then
/// focuses it once the file's threads are placed.
#[test]
fn following_a_thread_whose_file_isnt_diffed_yet_waits_for_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    let json = review_json("src/zz9.rs");
    let whole = r#"{"id":"z1","path":"src/zz9.rs","text":"On the far file","created":"2026-11-17T09:00:00Z"}"#;
    let json = format!("{},{whole}]}}", json.strip_suffix("]}").expect("a list"));
    std::fs::write(&path, json).expect("write");
    app.event(Event::Comments);
    screen(&mut app);
    key(&mut app, 'i');
    for _ in 0..4 {
        key(&mut app, 'j');
    }
    assert_eq!(selected(&app), "z1");
    assert_eq!(at(&app), (11, String::new()));
    assert!(status(&mut app).contains("waiting for this file's diff"));
    let far = diffed(&app, 11, diff("src/zz9.rs"));
    app.event(Event::Diffed(far));
    assert_eq!(at(&app), (11, "z1".to_owned()));
}

/// The comments beside the diff: a header per file (its folder dimmed,
/// its name bold, its threads), then a row per thread: its mark, its
/// line, the author when the row above in the file has another, the
/// first line, and its replies at the right (`↩1`). A thread whose lines
/// weren't found says `moved`, unless its file isn't in the diff. A
/// narrow panel leaves out the line, then the replies.
#[test]
fn the_comments_panel_lists_one_quiet_row_per_thread() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = reviewed(&dir, &review_json("src/zz9.rs"), (130, 22));
    app.tree_width = Some(46);
    app.relayout();
    key(&mut app, 'i');
    insta::assert_snapshot!("comment_list", screen(&mut app));
    let narrow: Vec<String> = [26, 24, 22, 20]
        .into_iter()
        .map(|width| {
            app.tree_width = Some(width);
            app.relayout();
            left(&mut app)
                .lines()
                .take(11)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();
    insta::assert_snapshot!("comment_list_narrow", narrow.join("\n\n"));
}

/// A thread's row has its mark in its look's role, its line in the
/// line numbers', `moved` dimmed in the warning's, and its author bold;
/// a resolved one is dimmed whole, its author still bold. File names
/// are bold.
#[test]
fn comment_rows_take_their_roles() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = reviewed(&dir, &review_json("src/zz9.rs"), (130, 22));
    app.tree_width = Some(46);
    app.relayout();
    key(&mut app, 'i');
    // Distinct styles for the roles the marks and rows take.
    app.theme = Theme::parse(
        "roles",
        "warn = yellow underline\nhint = cyan underline\ndim = dim underline\n\
             line-number = blue underline\n",
        &mut Vec::new(),
    );
    app.invalidate();
    let ink = |role: Role, attributes: Attributes, label: &str| {
        let ink = Ink {
            role,
            syntax: None,
            attributes,
        };
        (app.theme.ink_style(ink), label.to_owned())
    };
    let styles = [
        ink(Role::Warn, Attributes::NONE, "warn"),
        ink(Role::Hint, Attributes::NONE, "hint"),
        ink(Role::Dim, Attributes::NONE, "dim"),
        ink(Role::LineNumber, Attributes::NONE, "line-number"),
        ink(Role::Warn, Attributes::DIM, "warn+dim"),
        ink(Role::Dim, Attributes::BOLD, "dim+bold"),
        ink(Role::Plain, Attributes::BOLD, "bold"),
    ];
    let markup = style_markup(&mut app, &styles, 46);
    insta::assert_snapshot!(
        "comment_list_roles",
        markup
            .lines()
            .skip(1)
            .take(9)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Moving through the comments shows each thread in the diff, focused,
/// loading its file when it's another, while the keys stay in the
/// panel; opening it moves nothing. A thread on a file not in the diff
/// says so and leaves nothing focused. The wheel and a tap move the
/// selection too.
#[test]
fn moving_through_the_comments_shows_each_thread_in_the_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
    assert_eq!(selected(&app), "f1");
    assert_eq!(at(&app), (0, String::new()), "opening moves nothing");
    let mut seen = Vec::new();
    for _ in 0..5 {
        key(&mut app, 'j');
        seen.push(at(&app));
    }
    let at_ = |file: usize, root: &str| (file, root.to_owned());
    assert_eq!(
        seen,
        [
            at_(0, "d1"),
            at_(0, "p1"),
            at_(0, "r1"),
            at_(11, "m1"),
            at_(11, "")
        ]
    );
    screen(&mut app);
    assert_eq!(
        note(&mut app).as_deref(),
        Some("r reply · a ask · x resolve · f fix · e edit · d delete")
    );
    assert_eq!(app.pane, Pane::Tree);
    key(&mut app, 'k');
    assert_eq!(at(&app), at_(11, "m1"));
    key(&mut app, 'g');
    assert_eq!(at(&app), at_(0, "f1"));
    app.event(Event::Input(Input::Scroll {
        delta: 1,
        x: 5,
        y: 5,
    }));
    assert_eq!((selected(&app), at(&app)), ("d1".to_owned(), at_(0, "d1")));
    // Under the header: f1, d1, p1, then r1.
    app.event(Event::Input(Input::Click { x: 5, y: 5 }));
    assert_eq!((selected(&app), at(&app)), ("r1".to_owned(), at_(0, "r1")));
    // The selection follows `m` in the diff.
    press(&mut app, Key::Tab);
    key(&mut app, 'm');
    assert_eq!(at(&app), at_(0, "p1"));
    screen(&mut app);
    assert_eq!(selected(&app), "p1");
}

/// Following a folded thread from the comments shows it whole until the
/// selection moves on. `z` on it in the diff keeps what it sets, a
/// reply keeps it whole, and closing the panel ends the peek.
#[test]
fn following_unfolds_a_folded_thread_while_its_selected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
    // f1, d1, then p1, outdated and posted, and r1, resolved.
    key(&mut app, 'j');
    key(&mut app, 'j');
    assert_eq!(selected(&app), "p1");
    assert!(!app.doc.fold().collapsed("p1"));
    key(&mut app, 'j');
    assert!(app.doc.fold().collapsed("p1") && !app.doc.fold().collapsed("r1"));
    press(&mut app, Key::Tab);
    key(&mut app, 'z');
    assert!(app.doc.fold().collapsed("r1"));
    press(&mut app, Key::Tab);
    key(&mut app, 'k');
    key(&mut app, 'j');
    assert_eq!(selected(&app), "r1");
    assert!(app.doc.fold().collapsed("r1"), "as z left it");
    key(&mut app, 'k');
    press(&mut app, Key::Tab);
    key(&mut app, 'r');
    assert!(app.doc.writing().is_some());
    press(&mut app, Key::Esc);
    press(&mut app, Key::Tab);
    key(&mut app, 'j');
    assert_eq!(selected(&app), "r1");
    assert!(!app.doc.fold().collapsed("p1"), "replied in");
    key(&mut app, 'i');
    assert!(app.panel.is_none());
    assert!(app.doc.fold().collapsed("r1"));
}

/// `enter` or `tab` hands the keys to the diff, the thread focused for
/// `r`, `x`, `e`, `d` and `o`; `tab` comes back to the comments, and
/// `esc` in them hands the keys to the diff as it does from the tree.
#[test]
fn enter_or_tab_hands_the_keys_to_the_diff_on_the_thread() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
    let path = dir.path().join("comments.json");
    key(&mut app, 'j');
    press(&mut app, Key::Enter);
    assert_eq!((app.pane, at(&app)), (Pane::Diff, (0, "d1".to_owned())));
    key(&mut app, 'x');
    assert!(
        read_comments(&path)
            .comments
            .iter()
            .any(|c| c.id == "d1" && c.resolved)
    );
    press(&mut app, Key::Tab);
    assert!(app.pane == Pane::Tree && app.panel.is_some());
    assert!(screen(&mut app).starts_with("┌ Comments"));
    key(&mut app, 'j');
    let root = selected(&app);
    press(&mut app, Key::Tab);
    assert_eq!((app.pane, at(&app).1), (Pane::Diff, root));
    press(&mut app, Key::Tab);
    press(&mut app, Key::Esc);
    assert!(app.pane == Pane::Diff && app.panel.is_some());
}

/// The thread's keys are its table's: rebound, the new key acts and
/// the status bar names it; unbound, the old one acts as it would
/// without a thread.
#[test]
fn thread_keys_follow_the_thread_table() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
    let path = dir.path().join("comments.json");
    let resolved = || (read_comments(&path).comments.iter()).any(|c| c.id == "g1" && c.resolved);
    for binding in ["thread:R=resolve", "thread:x=unbind"] {
        app.keys.bind(binding).expect("binds");
    }
    for _ in 0..5 {
        key(&mut app, 'j');
    }
    screen(&mut app);
    assert_eq!(
        note(&mut app).as_deref(),
        Some("r reply · a ask · R resolve · f fix · e edit · d delete")
    );
    key(&mut app, 'x');
    assert!(!resolved());
    key(&mut app, 'R');
    assert!(resolved());
}

/// On a thread whose file isn't in the diff, the panel's keys act on
/// the selected row: `x` resolves it, `r` and `e` write in the editor,
/// as the diff has no box to show, and `d` twice deletes the draft,
/// then the thread. The status bar lists them; `d` then a move keeps
/// the draft, and the search still takes every letter.
#[test]
fn the_comments_panel_acts_on_a_thread_not_in_the_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
    let path = dir.path().join("comments.json");
    let find = |id: &str| {
        read_comments(&path)
            .comments
            .into_iter()
            .find(|c| c.id == id)
    };
    for _ in 0..5 {
        key(&mut app, 'j');
    }
    assert_eq!(
        (selected(&app), at(&app).1),
        ("g1".to_owned(), String::new())
    );
    screen(&mut app);
    assert_eq!(
        note(&mut app).as_deref(),
        Some("r reply · a ask · x resolve · f fix · e edit · d delete")
    );
    app.resize(200, 30);
    let shown = status(&mut app);
    assert!(
        shown.contains("r reply") && shown.contains("c filter"),
        "{shown}"
    );
    key(&mut app, 'x');
    assert!(find("g1").is_some_and(|c| c.resolved));
    assert_eq!((app.pane, selected(&app)), (Pane::Tree, "g1".to_owned()));
    app.message = None;
    screen(&mut app);
    assert!(note(&mut app).is_some_and(|n| n.contains("x reopen")));
    key(&mut app, 'x');
    assert!(find("g1").is_some_and(|c| !c.resolved));

    assert_eq!(
        press(&mut app, Key::Char('r')),
        Some(Action::Compose(String::new()))
    );
    app.composed(Some("Noted".to_owned()));
    let reply = read_comments(&path)
        .comments
        .into_iter()
        .find(|c| c.text == "Noted")
        .expect("the reply");
    assert_eq!(reply.in_reply_to.as_deref(), Some("g1"));
    assert_eq!(
        press(&mut app, Key::Char('e')),
        Some(Action::Compose("Noted".to_owned()))
    );
    app.composed(Some("Noted!".to_owned()));
    assert_eq!(find(&reply.id).map(|c| c.text), Some("Noted!".to_owned()));

    key(&mut app, 'd');
    key(&mut app, 'k');
    key(&mut app, 'd');
    assert!(find("m1").is_some(), "a move disarms d");
    key(&mut app, 'j');
    key(&mut app, 'd');
    key(&mut app, 'd');
    assert!(find(&reply.id).is_none() && find("g1").is_some());
    key(&mut app, 'o');
    assert!(screen(&mut app).contains("this thread has no link"));
    key(&mut app, 'd');
    key(&mut app, 'd');
    assert!(find("g1").is_none());
    assert!(!left(&mut app).contains("Gone"));

    let before = read_comments(&path);
    app.message = None;
    key(&mut app, '/');
    assert_eq!(selected(&app), "m1");
    assert!(
        !note(&mut app).is_some_and(|n| n.contains("r reply")),
        "typing"
    );
    typed(&mut app, "rexdo");
    assert!(app.panel.as_ref().is_some_and(|p| p.editing));
    assert!(left(&mut app).contains("no thread matches /rexdo"));
    assert_eq!(read_comments(&path), before, "the search takes them");
}

/// On a thread the diff shows, the panel's `r` and `e` open the box in
/// the diff, which takes the keys, and `x` acts without leaving the
/// panel.
#[test]
fn the_comments_panel_writes_in_the_diff_on_a_thread_shown_there() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
    let path = dir.path().join("comments.json");
    assert_eq!(selected(&app), "f1");
    assert_eq!(at(&app).1, "", "opening follows nothing");
    key(&mut app, 'j');
    assert_eq!(selected(&app), "d1");
    key(&mut app, 'x');
    assert!(
        read_comments(&path)
            .comments
            .iter()
            .any(|c| c.id == "d1" && c.resolved)
    );
    assert_eq!(app.pane, Pane::Tree);
    assert_eq!(press(&mut app, Key::Char('r')), None);
    let writes = |app: &App| app.doc.writing().map(|w| w.at.clone());
    assert_eq!(writes(&app), Some(Writes::Reply("d1".to_owned())));
    assert_eq!(app.pane, Pane::Diff);
    typed(&mut app, "Sure");
    press(&mut app, Key::Esc);
    assert!(
        read_comments(&path)
            .comments
            .iter()
            .any(|c| c.text == "Sure" && c.in_reply_to.as_deref() == Some("d1"))
    );
    // The thread the panel opens on isn't followed until it writes.
    key(&mut app, 'i');
    app.doc.set_focus(None);
    key(&mut app, 'i');
    assert_eq!(
        (selected(&app), at(&app).1),
        ("f1".to_owned(), String::new())
    );
    key(&mut app, 'e');
    assert!(matches!(writes(&app), Some(Writes::Edit { root, .. }) if root == "f1"));
}

/// `c` steps through All, Open, Drafts and Resolved, `C` back; the title
/// counts what's kept of every thread and names the filter. `/` keeps
/// the threads whose path, author or text has what's typed, with the
/// filter; `esc` clears it, then hands the keys to the diff. A filter
/// that keeps nothing says so.
#[test]
fn the_comments_panel_filters_by_state_and_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = listed(&dir, &review_json("src/zz9.rs"), (130, 30));
    app.tree_width = Some(46);
    app.relayout();
    let title = |app: &mut App| screen(app).lines().next().unwrap_or_default().to_owned();
    assert!(title(&mut app).starts_with("┌ Comments · 6 threads · 5 open ─"));
    let texts = |app: &mut App| {
        let shown = left(app);
        [
            "On the whole",
            "Why two?",
            "Fifty?",
            "Old four",
            "Moved away",
            "Gone",
        ]
        .into_iter()
        .filter(|t| shown.contains(t))
        .collect::<Vec<_>>()
    };
    key(&mut app, 'c');
    assert_eq!(
        texts(&mut app),
        ["On the whole", "Why two?", "Fifty?", "Moved away", "Gone"]
    );
    assert!(title(&mut app).starts_with("┌ Comments · 5/6 · open ─"));
    key(&mut app, 'c');
    assert_eq!(
        texts(&mut app),
        ["On the whole", "Why two?", "Moved away", "Gone"]
    );
    key(&mut app, 'c');
    assert_eq!(texts(&mut app), ["Old four"]);
    assert_eq!(selected(&app), "r1");
    assert_eq!(at(&app).1, "r1", "the diff follows the selection");
    key(&mut app, 'C');
    key(&mut app, 'C');
    typed(&mut app, "/octo");
    assert_eq!(texts(&mut app), ["Fifty?"], "the author");
    assert!(screen(&mut app).contains("└« /octo▏ ─"));
    press(&mut app, Key::Esc);
    typed(&mut app, "/zz9");
    assert_eq!(texts(&mut app), ["Moved away"], "the path");
    let file = app.doc.document.pos.file;
    press(&mut app, Key::Down);
    app.event(Event::Input(Input::Key(Key::Char('n'), Mods::CTRL)));
    assert_eq!(
        app.doc.document.pos.file, file,
        "typing leaves the tree alone"
    );
    press(&mut app, Key::Enter);
    assert!(title(&mut app).starts_with("┌ Comments · 1/6 · open · /zz9 ─"));
    key(&mut app, 'c');
    key(&mut app, 'c');
    assert!(
        left(&mut app).contains("no thread matches"),
        "the filter keeps the search"
    );
    key(&mut app, 'C');
    key(&mut app, 'C');
    press(&mut app, Key::Esc);
    assert_eq!(app.pane, Pane::Tree, "esc clears the search first");
    typed(&mut app, "/because");
    assert_eq!(texts(&mut app), ["Why two?"], "a reply's text");
    press(&mut app, Key::Enter);
    press(&mut app, Key::Esc);
    for _ in 0..4 {
        key(&mut app, 'c');
    }
    assert_eq!(texts(&mut app).len(), 5, "round to Open");
    press(&mut app, Key::Esc);
    assert_eq!(app.pane, Pane::Diff);
}

/// A resolved thread with a draft reply in it is a draft: first among
/// its file's, and kept by Drafts as well as Resolved, since posting
/// sends it.
#[test]
fn a_resolved_thread_with_a_draft_in_it_lists_as_a_draft() {
    let dir = tempfile::tempdir().expect("tempdir");
    let json = review_json("src/zz9.rs");
    let reply = r#"{"id":"r2","path":"src/some/deeply/nested/module/file.rs","text":"Reopen?","author":"","created":"2026-11-17T11:00:00Z","in_reply_to":"r1"}"#;
    let json = format!("{},{reply}]}}", json.strip_suffix("]}").expect("a list"));
    let mut app = listed(&dir, &json, (100, 40));
    let mut seen = vec![selected(&app)];
    for _ in 0..3 {
        key(&mut app, 'j');
        seen.push(selected(&app));
    }
    assert_eq!(seen, ["f1", "d1", "r1", "p1"]);
    let listed = |app: &mut App| left(app).contains("Old four");
    key(&mut app, 'c');
    assert!(!listed(&mut app), "not open");
    key(&mut app, 'c');
    assert!(listed(&mut app), "a draft");
    key(&mut app, 'c');
    assert!(listed(&mut app), "resolved");
}

/// Comments read while the file's diff is being made go to the pool
/// before the next frame, and come placed with the diff: the frame that
/// shows the diff shows its threads.
#[test]
fn comments_read_while_a_diff_runs_come_with_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(diff(FILE).change),
        (100, 20),
        Indicators::Classic,
    );
    app.event(Event::EnumerationDone(None));
    app.draw();
    assert!(
        app.jobs()
            .iter()
            .any(|j| matches!(j, Job::Diff { file: 0, .. }))
    );
    let path = dir.path().join("comments.json");
    std::fs::write(&path, review_json("src/zz9.rs")).expect("write");
    app.set_comments(path);
    let annotations = app.take_annotations().expect("for the pool");
    assert!(app.take_annotations().is_none(), "taken once");
    let loaded = loaded(diff(FILE));
    app.event(Event::Diffed(Done::Diff {
        file: 0,
        version: app.version(0),
        placed: crate::tui::worker::place_on(&annotations, &loaded, false),
        result: Ok(Box::new(loaded)),
    }));
    app.draw();
    assert!(app.screen.text().contains("Why two?"));
}

/// A file off screen, diffed for its comments, has their lost code
/// parsed with the diff, which no frame waits for: dropped from memory
/// after, it isn't diffed again for it.
#[test]
fn a_far_file_is_painted_with_its_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    let far = 11;
    let asked = |app: &App| {
        (app.jobs().into_iter()).find_map(|j| match j {
            Job::Diff { file, paint, .. } if file == far => Some(paint),
            _ => None,
        })
    };
    app.draw();
    assert_eq!(asked(&app), Some(true));
    let loaded = loaded(diff("src/zz9.rs"));
    let annotations = app.comments.as_ref().map(Comments::annotations);
    let placed = annotations.and_then(|a| crate::tui::worker::place_on(a, &loaded, true));
    app.event(Event::Diffed(Done::Diff {
        file: far,
        version: app.version(far),
        placed,
        result: Ok(Box::new(loaded)),
    }));
    app.doc.document.evict(0, &app.visible);
    app.draw();
    assert_eq!(asked(&app), None, "placed and painted");
}

/// The diff of a file comes with its comments placed but the code of
/// those lost not parsed yet: a placement of its own paints it after,
/// once.
#[test]
fn lost_code_is_painted_after_the_diff_lands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        Some(diff(FILE).change),
        (100, 20),
        Indicators::Classic,
    );
    app.event(Event::EnumerationDone(None));
    app.draw();
    let path = dir.path().join("comments.json");
    let lost = format!(
        r#"{{"version":1,"comments":[{{"id":"l1","path":"{FILE}","side":"new","start":{{"old":null,"new":9}},"end":{{"old":null,"new":9}},"text":"Gone","author":"","created":"2026-11-17T10:00:00Z","anchor":{{"before":[],"lines":["fn gone() -> u32 {{ 1 }}"],"after":[]}}}}]}}"#
    );
    std::fs::write(&path, lost).expect("write");
    app.set_comments(path);
    let annotations = app.take_annotations().expect("for the pool");
    let painted = |app: &App| {
        let threads = (app.comments.as_ref()).and_then(|c| c.threads(FILE.as_bytes()));
        let snippet = threads.and_then(|t| t.first()?.snippet.clone());
        let rows = snippet.expect("lost, with its code").rows;
        rows.iter()
            .flat_map(|r| &r.pieces)
            .any(|(s, _)| s.is_some())
    };
    let loaded = loaded(diff(FILE));
    app.event(Event::Diffed(Done::Diff {
        file: 0,
        version: app.version(0),
        placed: crate::tui::worker::place_on(&annotations, &loaded, false),
        result: Ok(Box::new(loaded)),
    }));
    assert!(!painted(&app), "not parsed with the diff");
    app.draw();
    assert!(place(&mut app), "a placement paints it");
    assert!(painted(&app));
    app.draw();
    assert!(!place(&mut app), "once");
}

/// A comment on lines written elsewhere is placed by the pool, asked for
/// right after the diffs on screen; a placement made before the
/// comments changed again, or on a diff since replaced, is dropped and
/// asked for again.
#[test]
fn placements_from_the_pool_land_unless_stale() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    let with = |extra: &[&str]| {
        let json = review_json("src/zz9.rs");
        let json = json.strip_suffix("]}").expect("a list");
        let added = extra.join(",");
        std::fs::write(&path, format!("{json},{added}]}}")).expect("write");
    };
    let comment = |id: &str, line: usize, text: &str| {
        format!(
            r#"{{"id":"{id}","path":"{FILE}","side":"new","start":{{"old":null,"new":{line}}},"end":{{"old":null,"new":{line}}},"text":"{text}","author":"","created":"2026-11-17T10:00:00Z","anchor":{{"before":[],"lines":["line {}"],"after":[]}}}}"#,
            line + 1
        )
    };
    let roots = |app: &App| -> Vec<String> {
        let threads = app
            .comments
            .as_ref()
            .and_then(|c| c.threads(FILE.as_bytes()));
        threads.map_or_else(Vec::new, |t| t.iter().map(|t| t.root.clone()).collect())
    };
    let place_job = |app: &mut App| {
        app.draw();
        let jobs = app.jobs();
        let at = jobs.iter().position(|j| matches!(j, Job::Place(_)));
        assert_eq!(at, Some(0), "first: the file shown is loaded");
        match jobs.into_iter().next() {
            Some(Job::Place(job)) => job,
            _ => unreachable!("checked above"),
        }
    };

    with(&[&comment("n1", 2, "New")]);
    app.event(Event::Comments);
    assert!(!roots(&app).contains(&"n1".to_owned()), "not placed yet");
    let stale = place_job(&mut app);
    with(&[&comment("n1", 2, "New"), &comment("n2", 5, "Newer")]);
    app.event(Event::Comments);
    app.event(Event::Diffed(stale.run()));
    assert!(!roots(&app).contains(&"n1".to_owned()), "placed before n2");

    let mut current = place_job(&mut app);
    let replaced = Arc::clone(&current.diff);
    current.diff = Arc::new(diff(FILE));
    app.event(Event::Diffed(current.run()));
    assert!(!roots(&app).contains(&"n1".to_owned()), "on another diff");
    let mut current = place_job(&mut app);
    assert!(Arc::ptr_eq(&current.diff, &replaced));
    current.diff = replaced;
    app.event(Event::Diffed(current.run()));
    let roots = roots(&app);
    assert!(roots.contains(&"n1".to_owned()) && roots.contains(&"n2".to_owned()));
}

/// `m` goes from thread to thread in the order the diff shows them,
/// focusing each, then on into the next file with comments, waiting for
/// its diff when it has none yet; `M` goes back across files too.
#[test]
fn m_steps_through_threads_across_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    screen(&mut app);
    let focused = |app: &App| {
        app.doc
            .focused()
            .map(|t| t.root.clone())
            .unwrap_or_default()
    };
    let mut seen = Vec::new();
    for _ in 0..4 {
        key(&mut app, 'm');
        seen.push(focused(&app));
        let shown = screen(&mut app);
        let first = app.doc.focused().map(|t| t.comments[0].text.clone());
        assert!(shown.contains(&first.unwrap_or_default()), "{shown}");
    }
    assert_eq!(seen, ["f1", "d1", "r1", "p1"]);
    key(&mut app, 'm');
    let far = 11;
    assert_eq!(app.doc.document.pos.file, far);
    assert!(screen(&mut app).contains("waiting for this file's diff"));
    let diffed = diffed(&app, far, diff("src/zz9.rs"));
    app.event(Event::Diffed(diffed));
    assert_eq!(focused(&app), "m1");
    key(&mut app, 'm');
    assert!(screen(&mut app).contains("no more threads below"));
    key(&mut app, 'M');
    assert_eq!(
        (app.doc.document.pos.file, focused(&app)),
        (0, "p1".to_owned())
    );
    key(&mut app, 'M');
    assert_eq!(focused(&app), "r1");
}

/// While a thread has the focus, `x` resolves and reopens it, `r` opens
/// a reply box in it, `e` edits your draft there, `d` twice deletes the
/// draft, and `esc` leaves. Without the focus the same keys keep their
/// own meaning: `r` the rendered view, `x` more lines.
#[test]
fn a_focused_thread_takes_its_keys_only_while_focused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    let find = |id: &str| {
        read_comments(&path)
            .comments
            .into_iter()
            .find(|c| c.id == id)
    };
    screen(&mut app);
    key(&mut app, 'r');
    assert!(screen(&mut app).contains("no renderer for .rs"));
    key(&mut app, 'x');
    assert!(app.doc.writing().is_none());
    assert!(find("d1").is_some_and(|c| !c.resolved));

    key(&mut app, 'm');
    key(&mut app, 'm');
    assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("d1"));
    app.message = None;
    let shown = screen(&mut app);
    // The most used first, so a narrow status bar keeps them.
    assert!(
        shown.contains("r reply · a ask · x resolve · f fix · e edit · d …"),
        "{shown}"
    );
    let hint = note(&mut app).unwrap_or_default();
    assert_eq!(
        hint, "r reply · a ask · x resolve · f fix · e edit · d delete",
        "no fold or esc"
    );
    key(&mut app, 'x');
    assert!(find("d1").is_some_and(|c| c.resolved));
    key(&mut app, 'x');
    assert!(find("d1").is_some_and(|c| !c.resolved));

    key(&mut app, 'r');
    typed(&mut app, "Sure");
    insta::assert_snapshot!("comment_reply", screen(&mut app));
    press(&mut app, Key::Esc);
    let reply = read_comments(&path)
        .comments
        .into_iter()
        .find(|c| c.text == "Sure")
        .expect("the reply");
    assert_eq!(reply.in_reply_to.as_deref(), Some("d1"));
    assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("d1"));

    key(&mut app, 'e');
    typed(&mut app, "!");
    press(&mut app, Key::Esc);
    assert_eq!(find(&reply.id).map(|c| c.text), Some("Sure!".to_owned()));
    key(&mut app, 'd');
    assert!(find(&reply.id).is_some(), "one d asks");
    key(&mut app, 'd');
    assert!(find(&reply.id).is_none() && find("d1").is_some());
    key(&mut app, 'o');
    assert!(screen(&mut app).contains("this thread has no link"));
    press(&mut app, Key::Esc);
    assert!(app.doc.focused().is_none());

    // Scrolled off screen, the thread lets go: `x` reveals lines again.
    assert!(app.doc.go_to_thread("d1"));
    press(&mut app, Key::PageDown);
    key(&mut app, 'x');
    assert!(app.doc.focused().is_none());
    assert!(find("d1").is_some_and(|c| !c.resolved));
}

/// A thread placed on lines a `⋯` separator hides reveals them, two
/// lines around them and no more: it hangs under its own line, between
/// a separator for the hidden lines above and one for those below, and
/// the view stays on the row it showed. `X` and back keeps them.
#[test]
fn a_thread_on_hidden_lines_reveals_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    std::fs::write(
            &path,
            r#"{"version":1,"comments":[{"id":"h1","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":20,"new":20},"end":{"old":20,"new":20},"text":"Hidden here","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":[],"lines":["line 20"],"after":[]}}]}"#,
        )
        .expect("write");
    let mut app = app(100, 12);
    // The `@@` row and the first hunk's eight, then the separator.
    app.doc.scroll(9);
    let top = |app: &mut App| screen(app).lines().nth(1).unwrap_or_default().to_owned();
    assert!(top(&mut app).contains("⋯ 39 unchanged lines"));
    let line = app.doc.line_at_top();
    app.set_comments(path);
    assert_eq!(app.doc.line_at_top(), line, "the view stays");
    let shown = screen(&mut app);
    assert!(top(&mut app).contains("⋯ 10 unchanged lines"), "{shown}");
    assert!(shown.contains("⋯ 24 unchanged lines"), "{shown}");
    let has_line = |n: usize| shown.contains(&format!("│ line {n}"));
    assert!(
        (18..=22).all(has_line) && !has_line(17) && !has_line(23),
        "{shown}"
    );
    let under = shown
        .lines()
        .skip_while(|l| !l.contains("│ line 20"))
        .nth(1)
        .unwrap_or_default();
    assert!(under.contains("▾ you · 2h ago ─"), "{shown}");
    key(&mut app, 'X');
    key(&mut app, 'X');
    assert!(screen(&mut app).contains("⋯ 10 unchanged lines"));
    key(&mut app, 'm');
    let shown = screen(&mut app);
    let under = shown
        .lines()
        .skip_while(|l| !l.contains("│ line 20"))
        .nth(1)
        .unwrap_or_default();
    assert!(under.contains("▾ you · 2h ago ━"), "{shown}");
}

/// `o` opens a posted thread on its host. Its comments are the host's:
/// `e` and `d` say so. Replying to an outdated thread says the review
/// plugin's warning once, when it has one.
#[test]
fn posted_threads_open_on_their_host_and_outdated_ones_warn() {
    let quiet = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&quiet);
    screen(&mut app);
    for _ in 0..4 {
        key(&mut app, 'm');
    }
    key(&mut app, 'r');
    assert!(!screen(&mut app).contains("outdated:"), "no warning to say");

    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    described(
        &mut app,
        r#"{"version":1,"command":"pr","noun":"pull request","nouns":"pull requests","outdated_reply_warning":"the host folds this thread, so a reply there is easy to miss"}"#,
    );
    screen(&mut app);
    for _ in 0..4 {
        key(&mut app, 'm');
    }
    assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("p1"));
    assert_eq!(
        press(&mut app, Key::Char('o')),
        Some(Action::Browse(
            "https://github.com/o/r/pull/1#discussion_r1".to_owned()
        ))
    );
    key(&mut app, 'e');
    assert!(screen(&mut app).contains("no draft here to edit"));
    key(&mut app, 'd');
    assert!(screen(&mut app).contains("no draft here to delete"));
    key(&mut app, 'r');
    assert!(screen(&mut app).contains("outdated: the host folds this thread"));
    press(&mut app, Key::Esc);
    key(&mut app, 'r');
    assert!(!screen(&mut app).contains("the host folds"), "once");
    press(&mut app, Key::Esc);
    assert_eq!(
        read_comments(&path).comments.len(),
        7,
        "empty replies discarded"
    );
}

/// `C`, or the tree menu's entry, writes a comment on the whole file in
/// a box under its first row.
#[test]
fn c_on_a_file_comments_on_the_whole_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = app(100, 20);
    app.set_comments(path.clone());
    choose_again(&mut app, "file.rs", "Comment on file");
    assert_eq!(app.pane, Pane::Diff, "the box is in the diff");
    typed(&mut app, "Whole");
    let shown = screen(&mut app);
    let at = |text: &str| {
        shown
            .find(text)
            .unwrap_or_else(|| panic!("{text}:\n{shown}"))
    };
    assert!(at("Whole▏") < at("line 1"), "{shown}");
    esc(&mut app);
    let saved = read_comments(&path);
    let [comment] = saved.comments.as_slice() else {
        panic!("one comment: {saved:?}");
    };
    assert_eq!(
        (comment.path.as_str(), comment.text.as_str()),
        (FILE, "Whole")
    );
    assert!(comment.start.is_none() && comment.anchor.is_none());
    assert_eq!(app.doc.focused().map(|t| &t.at), Some(&model::At::File));
    key(&mut app, 'C');
    assert_eq!(app.doc.writing().map(|w| &w.at), Some(&Writes::File));
    esc(&mut app);
    // A file not diffed yet takes one too: it needs no lines.
    choose_again(&mut app, "zz.rs", "Comment on file");
    typed(&mut app, "Later");
    assert!(screen(&mut app).contains("Later▏"));
    esc(&mut app);
    assert!(
        read_comments(&path)
            .comments
            .iter()
            .any(|c| c.path == "src/zz.rs")
    );
}

/// `z` folds the focused thread to one row and back; `Z` folds all.
#[test]
fn z_folds_threads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    screen(&mut app);
    key(&mut app, 'm');
    key(&mut app, 'm');
    key(&mut app, 'z');
    let shown = screen(&mut app);
    assert!(
        shown.contains("▸ you · 2h ago · Why two?") && shown.contains("1 reply"),
        "{shown}"
    );
    assert!(!shown.contains("Because."));
    key(&mut app, 'z');
    assert!(screen(&mut app).contains("Because."));
    key(&mut app, 'Z');
    let shown = screen(&mut app);
    assert!(
        !shown.contains('▾') && shown.contains("▸ you · 2h ago · resolved · Old four"),
        "{shown}"
    );
}

/// `c` in the tree keeps only the files with comments, and `f` keeps
/// to them; `esc` shows every file again.
#[test]
fn the_tree_keeps_the_files_with_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    screen(&mut app);
    press(&mut app, Key::Tab);
    key(&mut app, 'c');
    let shown = screen(&mut app);
    assert!(shown.contains("Files · 2/12"), "{shown}");
    assert!(
        !shown.contains("zz0.rs") && shown.contains("zz9.rs"),
        "{shown}"
    );
    key(&mut app, 'f');
    assert_eq!(app.doc.document.pos.file, 11);
    press(&mut app, Key::Esc);
    assert!(screen(&mut app).contains("zz0.rs"));
}

fn placed(app: &App, root: &str) -> bool {
    (app.comments.as_ref())
        .and_then(|c| c.threads(FILE.as_bytes()))
        .is_some_and(|threads| threads.iter().any(|t| t.root == root))
}

/// When another program changes the file, its threads are placed again
/// and its notice shows.
#[test]
fn a_changed_comments_file_is_read_again_with_its_notice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    screen(&mut app);
    assert!(placed(&app, "p1"));
    review::apply(
        &path,
        vec![
            Op::Rm(Target::Id("p1".to_owned())),
            Op::Notice(Some("comments as of 10:42".to_owned())),
        ],
    )
    .expect("applies");
    app.event(Event::Comments);
    let shown = screen(&mut app);
    assert!(shown.contains("comments as of 10:42"), "{shown}");
    assert!(!placed(&app, "p1") && placed(&app, "d1"));
}

/// A review plugin's progress reads the comments file again, without
/// waiting on the file watcher: it writes as it goes.
#[test]
fn a_review_runs_progress_reads_the_comments_file_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    screen(&mut app);
    assert!(placed(&app, "p1"));
    review::apply(&path, vec![Op::Rm(Target::Id("p1".to_owned()))]).expect("applies");
    let progress = Said::Progress("page 2".to_owned());
    app.event(Event::Review {
        id: 1,
        said: progress,
    });
    assert!(!placed(&app, "p1") && placed(&app, "d1"));
}

/// Only a review session's file has drafts: your comments not posted
/// in it. In a local review nothing is posted, so nothing is a draft,
/// and an agent's reply in a session, with how the agent ran, is left for
/// you, marked, rather than posted under your name.
#[test]
fn drafts_are_yours_in_a_review_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    review::apply(
        &path,
        vec![Op::Add {
            comment: Box::new(review::Comment {
                path: FILE.to_owned(),
                text: "From the agent".to_owned(),
                author: "claude".to_owned(),
                label: Some("read-only sandbox".to_owned()),
                ..review::Comment::default()
            }),
            reply_to: Some(Target::Id("d1".to_owned())),
        }],
    )
    .expect("applies");
    app.event(Event::Comments);
    let shown = screen(&mut app);
    assert!(!shown.contains("draft") && !shown.contains('◆'), "{shown}");
    assert!(!shown.contains("· private"), "no origin words: {shown}");
    assert_eq!(app.comments.as_ref().map(Comments::counts), Some((6, 0)));

    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(&mut app, prepare, Said::Done(Ok(())));
    let shown = screen(&mut app);
    assert!(shown.contains("▾ you · 2h ago · draft"), "{shown}");
    assert!(
        shown.contains("claude · just now · read-only sandbox · private · A sh"),
        "{shown}"
    );
    assert_eq!(app.comments.as_ref().map(Comments::counts), Some((6, 5)));
    assert_eq!(
        app.sending(true).comments,
        5,
        "the agent's reply isn't sent"
    );

    // `A` on its thread makes it yours: a draft, sent with the rest.
    app.reviewing.job = None;
    app.follow_thread("d1");
    key(&mut app, 'A');
    assert!(status(&mut app).contains("now yours: 1 draft"));
    assert_eq!(app.sending(true).comments, 6);
    let authors: Vec<String> = (read_comments(&path).comments.into_iter())
        .map(|c| c.author)
        .collect();
    assert!(!authors.contains(&"claude".to_owned()), "{authors:?}");
}

/// A review session's file: a resolved thread of yours with an agent's
/// reply, an open one of yours, and a resolved thread on the host holding
/// your draft reply and the host's.
fn resolved_mix() -> String {
    let c = |id: &str, author: &str, rest: &str| {
        format!(
            r#"{{"id":"{id}","path":"{FILE}","text":"{id}","author":"{author}","created":"2026-11-17T10:00:00Z"{rest}}}"#
        )
    };
    let comments = [
        c("l1", "", r#","resolved":true"#),
        c("l2", "claude", r#","resolved":false,"in_reply_to":"l1""#),
        c("o1", "", r#","resolved":false"#),
        c("h1", "octocat", r#","resolved":true,"ref":"github:1""#),
        c("h2", "", r#","resolved":false,"in_reply_to":"h1""#),
        c(
            "h3",
            "ana",
            r#","resolved":false,"in_reply_to":"h1","ref":"github:2""#,
        ),
        // Yours, deleted on the host, under a reply still there.
        c(
            "g1",
            "",
            r#","resolved":true,"post_error":"deleted on the host""#,
        ),
        c(
            "g2",
            "ana",
            r#","resolved":false,"in_reply_to":"g1","ref":"github:3""#,
        ),
    ];
    format!(r#"{{"version":1,"comments":[{}]}}"#, comments.join(","))
}

/// `clear_resolved` asks first, naming what goes, and any key but `y`
/// keeps it all. `y` deletes the resolved threads with nothing on the
/// host, and from one on the host only what isn't posted, saying the
/// host's threads stay; open threads stay.
#[test]
fn clearing_resolved_threads_deletes_only_what_lives_here() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = reviewed(&dir, &resolved_mix(), (120, 30));
    let path = dir.path().join("comments.json");
    let ids = || -> Vec<String> {
        (read_comments(&path).comments.into_iter())
            .map(|c| c.id)
            .collect()
    };
    let clear = |app: &mut App| {
        press(app, Key::Char(' '));
        key(app, '-');
        status(app)
    };
    let asked = "delete 1 resolved thread and 1 private comment in host threads? y/n";
    assert!(clear(&mut app).contains(asked));
    key(&mut app, 'n');
    assert!(status(&mut app).contains("kept"));
    assert_eq!(ids(), ["l1", "l2", "o1", "h1", "h2", "h3", "g1", "g2"]);

    assert!(clear(&mut app).contains(asked));
    key(&mut app, 'y');
    assert_eq!(ids(), ["o1", "h1", "h3", "g1", "g2"]);
    assert_eq!(
        note(&mut app).as_deref(),
        Some(
            "deleted 1 resolved thread and 1 private comment in host threads · host threads \
             stay; resolve hides them"
        )
    );
    assert!(clear(&mut app).contains("no resolved threads to delete · host threads stay"));
}

/// `d` twice on a resolved thread with nothing on the host deletes it
/// whole, not only its last comment.
#[test]
fn d_deletes_a_resolved_local_thread_whole() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = reviewed(&dir, &resolved_mix(), (120, 30));
    app.follow_thread("l1");
    key(&mut app, 'd');
    assert!(status(&mut app).contains("deletes this thread, replies too"));
    key(&mut app, 'd');
    let left: Vec<String> = (read_comments(&dir.path().join("comments.json")).comments)
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(left, ["o1", "h1", "h2", "h3", "g1", "g2"]);
}

/// The local review's file holds every branch's threads: the viewer
/// shows the current branch's and those on none, a new one goes on the
/// current branch, and the comments panel lists the others folded,
/// where `b` shows them and `V` brings one here.
#[test]
fn local_threads_keep_to_their_branch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let json = format!(
        r#"{{"version":1,"comments":[
{{"id":"h1","path":"{FILE}","side":"new","start":{{"old":null,"new":2}},"end":{{"old":null,"new":2}},"text":"Here","author":"","created":"2026-11-17T10:00:00Z","branch":"main","anchor":{{"before":[],"lines":["line 2"],"after":[]}}}},
{{"id":"o1","path":"{FILE}","side":"new","start":{{"old":null,"new":2}},"end":{{"old":null,"new":2}},"text":"Elsewhere","author":"","created":"2026-11-17T10:00:00Z","branch":"topic","anchor":{{"before":[],"lines":["line 2"],"after":[]}}}},
{{"id":"o2","path":"{FILE}","text":"Its reply","author":"","created":"2026-11-17T10:01:00Z","branch":"topic","in_reply_to":"o1"}},
{{"id":"n1","path":"{FILE}","text":"Everywhere","author":"","created":"2026-11-17T10:00:00Z"}}
]}}"#
    );
    std::fs::write(&path, json).expect("write");
    let mut app = app(200, 30);
    app.set_branch(Some("main".to_owned()));
    app.set_comments(path.clone());
    let shown = screen(&mut app);
    assert!(
        shown.contains("Here") && shown.contains("Everywhere"),
        "{shown}"
    );
    assert!(!shown.contains("Elsewhere"), "{shown}");
    assert_eq!(app.comments.as_ref().map(Comments::counts), Some((2, 0)));

    key(&mut app, 'c');
    typed(&mut app, "New one");
    press(&mut app, Key::Esc);
    let added = read_comments(&path)
        .comments
        .into_iter()
        .find(|c| c.text == "New one")
        .expect("saved");
    assert_eq!(added.branch.as_deref(), Some("main"));

    key(&mut app, 'i');
    let listed = left(&mut app);
    assert!(
        listed.contains("1 thread on other branches · b lists them"),
        "{listed}"
    );
    assert!(!listed.contains("Elsewhere"), "{listed}");
    key(&mut app, 'b');
    let listed = left(&mut app);
    assert!(
        listed.contains("Other branches · V moves one here") && listed.contains("topic"),
        "{listed}"
    );
    for _ in 0..10 {
        if selected(&app) == "o1" {
            break;
        }
        press(&mut app, Key::Down);
    }
    app.message = None;
    app.dirty = true;
    assert!(
        status(&mut app).contains("V move here"),
        "{}",
        status(&mut app)
    );
    // `B` is keep both's, in a conflict, and moves nothing.
    key(&mut app, 'B');
    let kept = read_comments(&path);
    assert!(
        kept.comments
            .iter()
            .filter(|c| c.id.starts_with('o'))
            .all(|c| c.branch.as_deref() == Some("topic"))
    );
    key(&mut app, 'V');
    let moved = read_comments(&path);
    assert!(
        moved
            .comments
            .iter()
            .filter(|c| c.id.starts_with('o'))
            .all(|c| c.branch.as_deref() == Some("main")),
        "the thread, its reply too"
    );
    press(&mut app, Key::Esc);
    assert!(screen(&mut app).contains("Elsewhere"));
}

/// A checkout moves the comments shown to the new branch's, and new
/// ones go on it.
#[test]
fn a_checkout_shows_the_threads_of_the_new_branch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let thread = |id: &str, text: &str, branch: &str| {
        format!(
            r#"{{"id":"{id}","path":"{FILE}","text":"{text}","author":"","created":"2026-11-17T10:00:00Z","branch":"{branch}"}}"#
        )
    };
    let json = format!(
        r#"{{"version":1,"comments":[{},{}]}}"#,
        thread("h1", "On main", "main"),
        thread("t1", "On topic", "topic")
    );
    std::fs::write(&path, json).expect("write");
    let mut app = app(200, 30);
    app.set_branch(Some("main".to_owned()));
    app.set_comments(path.clone());
    let shown = screen(&mut app);
    assert!(
        shown.contains("On main") && !shown.contains("On topic"),
        "{shown}"
    );
    app.event(Event::Branch(Some("topic".to_owned())));
    let shown = screen(&mut app);
    assert!(
        shown.contains("On topic") && !shown.contains("On main"),
        "{shown}"
    );
    key(&mut app, 'C');
    typed(&mut app, "New one");
    press(&mut app, Key::Esc);
    let added = (read_comments(&path).comments.into_iter())
        .find(|c| c.text == "New one")
        .expect("saved");
    assert_eq!(added.branch.as_deref(), Some("topic"));
}

/// "Suggest change" opens the box with the lines as a suggestion to
/// edit, the cursor above it; saved, the lines go in `suggestion`, and
/// `e` edits both.
#[test]
fn a_suggested_change_is_saved_as_the_comments_suggestion() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = app(100, 30);
    app.set_comments(path.clone());
    screen(&mut app);
    key(&mut app, 'S');
    let text = app.doc.writing().map(|w| w.text.text().to_owned());
    assert_eq!(
        text.as_deref(),
        Some("\n\n```suggestion\nline 1\n```"),
        "{}",
        screen(&mut app)
    );
    typed(&mut app, "Say one");
    press(&mut app, Key::Down);
    press(&mut app, Key::Down);
    press(&mut app, Key::Down);
    press(&mut app, Key::End);
    typed(&mut app, " (one)");
    press(&mut app, Key::Esc);
    let saved = read_comments(&path).comments;
    assert_eq!(
        (saved[0].text.as_str(), saved[0].suggestion.as_deref()),
        ("Say one", Some("line 1 (one)"))
    );
    assert!(screen(&mut app).contains("suggested change:"));
    key(&mut app, 'e');
    let text = app.doc.writing().map(|w| w.text.text().to_owned());
    assert_eq!(
        text.as_deref(),
        Some("Say one\n\n```suggestion\nline 1 (one)\n```")
    );
}

/// A comment written in the editor whose save fails opens in the box
/// again where the diff shows its place, else comes back to keep.
#[test]
fn text_from_the_editor_is_never_lost() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = app(100, 30);
    app.set_comments(path.clone());
    std::fs::write(&path, r#"{"version":2,"comments":[]}"#).expect("write");
    screen(&mut app);
    key(&mut app, ' ');
    assert!(matches!(
        press(&mut app, Key::Char('E')),
        Some(Action::Compose(_))
    ));
    assert_eq!(app.composed(Some("Written there".to_owned())), None);
    let text = app.doc.writing().map(|w| w.text.text().to_owned());
    assert_eq!(text.as_deref(), Some("Written there"), "back in the box");

    press(&mut app, Key::Esc);
    press(&mut app, Key::Esc);
    app.composing = Some(Compose::Reply("gone".to_owned()));
    assert_eq!(
        app.composed(Some("A reply".to_owned())),
        Some("A reply".to_owned()),
        "no box for a thread not shown: the caller keeps it"
    );
}

/// A signal to stop saves the box's text, as `ctrl+c` does; leaving a
/// review with drafts not posted says so.
#[test]
fn an_interrupt_saves_the_box_and_quitting_names_unposted_drafts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    screen(&mut app);
    key(&mut app, 'C');
    typed(&mut app, "Half written");
    app.interrupted();
    assert!(
        read_comments(&path)
            .comments
            .iter()
            .any(|c| c.text == "Half written")
    );
    assert_eq!(app.unposted_note(), None, "not a review");
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    assert_eq!(
        app.unposted_note().as_deref(),
        Some("PR #7: 6 drafts not posted")
    );
}

/// A reply whose thread leaves the file while it's written, as when an
/// import removes it, keeps its text: the box writes a comment on the
/// file from then on, and says so.
#[test]
fn a_reply_whose_thread_goes_keeps_its_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    key(&mut app, 'm');
    key(&mut app, 'm');
    assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("d1"));
    key(&mut app, 'r');
    typed(&mut app, "Keep me");
    let other = Op::Add {
        comment: Box::new(review::Comment {
            text: "Meanwhile".to_owned(),
            ..review::Comment::default()
        }),
        reply_to: Some(Target::Id("p1".to_owned())),
    };
    review::apply(&path, vec![other]).expect("added");
    app.event(Event::Comments);
    let writes = |app: &App| {
        app.doc
            .writing()
            .map(|w| (w.at.clone(), w.text.text().to_owned()))
    };
    let replying = Some((Writes::Reply("d1".to_owned()), "Keep me".to_owned()));
    assert_eq!(writes(&app), replying, "another thread changing leaves it");
    review::apply(&path, vec![Op::Rm(Target::Id("d1".to_owned()))]).expect("removed");
    app.event(Event::Comments);
    assert_eq!(writes(&app), Some((Writes::File, "Keep me".to_owned())));
    let shown = screen(&mut app);
    assert!(shown.contains("Keep me"), "{shown}");
    assert!(shown.contains("is gone"), "{shown}");
    press(&mut app, Key::Esc);
    let kept = read_comments(&path);
    let saved = kept
        .comments
        .iter()
        .find(|c| c.text == "Keep me")
        .expect("saved");
    assert!(
        saved.in_reply_to.is_none() && saved.end.is_none(),
        "{saved:?}"
    );
}

/// `enter` saves a new comment and leaves the keys to the diff, saying
/// that `m` goes to it, as `m` right after does: `s` there cycles the
/// layout rather than applying, and `f` goes to the next file. `esc`
/// keeps it focused, saying how to delete it; a reply keeps the focus.
#[test]
fn enter_saves_a_new_comment_and_leaves_the_keys_to_the_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = app(100, 30);
    app.set_comments(path.clone());
    screen(&mut app);
    key(&mut app, 'S');
    typed(&mut app, "Shorter");
    press(&mut app, Key::Enter);
    assert_eq!(read_comments(&path).comments.len(), 1);
    let shown = status(&mut app);
    assert!(shown.contains("comment saved · m goes to it"), "{shown}");
    assert_eq!(focused(&app), None);
    let layout = app.view;
    assert_eq!(press(&mut app, Key::Char('s')), None, "nothing applied");
    assert_ne!(app.view, layout, "s cycles the layout");
    key(&mut app, 'f');
    assert_eq!(app.doc.document.pos.file, 1, "f goes to the next file");

    key(&mut app, 'F');
    key(&mut app, 'c');
    typed(&mut app, "Second");
    press(&mut app, Key::Enter);
    screen(&mut app);
    key(&mut app, 'm');
    let second = read_comments(&path).comments[1].id.clone();
    assert_eq!(focused(&app), Some(second.as_str()), "m goes to it");

    esc(&mut app);
    key(&mut app, 'j');
    key(&mut app, 'c');
    typed(&mut app, "Kept");
    press(&mut app, Key::Esc);
    let kept = read_comments(&path).comments[2].id.clone();
    let shown = status(&mut app);
    assert!(shown.contains("draft kept · d d deletes"), "{shown}");
    screen(&mut app);
    assert_eq!(focused(&app), Some(kept.as_str()), "esc keeps it focused");
    key(&mut app, 'r');
    typed(&mut app, "And a reply");
    press(&mut app, Key::Enter);
    screen(&mut app);
    assert_eq!(
        focused(&app),
        Some(kept.as_str()),
        "a reply keeps the focus"
    );
}

/// `A` on a thread with nothing someone else wrote unposted says so,
/// rather than staging the lines under it.
#[test]
fn a_on_a_thread_with_nothing_to_adopt_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.follow_thread("d1");
    assert_eq!(press(&mut app, Key::Char('A')), None);
    let shown = status(&mut app);
    assert!(shown.contains("nothing here to make yours"), "{shown}");
}

/// `y` on a focused thread copies it as a prompt; text selected in one of
/// its comments is still `y`'s to copy.
#[test]
fn y_on_a_thread_copies_it_as_a_prompt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.follow_thread("d1");
    let copied = press(&mut app, Key::Char('y'));
    assert!(
        matches!(&copied, Some(Action::Copy { note, .. }) if note == "copied a thread as a prompt"),
        "{copied:?}"
    );
}

/// The file `diff` makes for `path`, its new side's first line `first`.
fn with_first_line(path: &str, first: &str) -> FileDiff {
    let mut fd = diff(path);
    let new =
        String::from_utf8_lossy(fd.new.bytes()).replacen("line 1\n", &format!("{first}\n"), 1);
    fd.new = Text::new(new.into_bytes());
    fd.hunks = hunks(&fd.old, &fd.new, &Options::default());
    fd
}

/// `s` on an applied suggestion takes it back: the lines it replaced go in
/// place of the suggested ones. Applied, then discarded another way, it
/// applies again from the lines it replaced.
#[test]
fn s_takes_an_applied_suggestion_back_and_applies_one_discarded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = app(100, 30);
    app.set_comments(path.clone());
    screen(&mut app);
    let original = diff(FILE);
    let changed = with_first_line(FILE, "line one");
    let first = review::LineNumbers {
        old: None,
        new: Some(0),
    };
    let suggested = Op::Add {
        comment: Box::new(review::Comment {
            path: FILE.to_owned(),
            start: Some(first),
            end: Some(first),
            anchor: Some(Anchor::new(&original.new, 0..1)),
            text: "Spell it out".to_owned(),
            suggestion: Some("line one".to_owned()),
            ..review::Comment::default()
        }),
        reply_to: None,
    };
    review::apply(&path, vec![suggested]).expect("applies");
    app.event(Event::Comments);
    screen(&mut app);
    let root = read_comments(&path).comments[0].id.clone();
    let s = |app: &mut App| {
        app.follow_thread(&root);
        match press(app, Key::Char('s')) {
            Some(Action::ApplySuggestion(suggestion)) => {
                (suggestion.anchor.lines, suggestion.lines)
            }
            other => panic!("{other:?}: {}", status(app)),
        }
    };
    let lines = |l: &[&str]| l.iter().map(|&l| l.to_owned()).collect::<Vec<_>>();
    // The list read again after a write, as the loop has it read.
    let reload = |app: &mut App, fd: FileDiff| {
        app.doc.document.restale();
        let again = diffed(app, 0, fd);
        app.event(Event::Diffed(again));
        screen(app);
    };
    // Applies, as the loop writes it, and the list read again shows it.
    let apply = |app: &mut App| {
        assert_eq!(s(app), (lines(&["line 1"]), lines(&["line one"])));
        app.event(Event::Applied(Ok(annotation::Applied {
            replaced: 0..1,
            lines: 0..1,
            anchor: Some(Anchor::new(&changed.new, 0..1)),
        })));
        reload(app, with_first_line(FILE, "line one"));
    };
    apply(&mut app);
    let marked = &read_comments(&path).comments[0];
    assert_eq!(marked.replaced.as_deref(), Some("line 1"), "{marked:?}");
    app.follow_thread(&root);
    app.message = None;
    app.dirty = true;
    let shown = status(&mut app);
    assert!(shown.contains("s revert"), "{shown}");

    assert_eq!(s(&mut app), (lines(&["line one"]), lines(&["line 1"])));
    app.event(Event::Applied(Ok(annotation::Applied {
        replaced: 0..1,
        lines: 0..1,
        anchor: Some(Anchor::new(&original.new, 0..1)),
    })));
    let shown = status(&mut app);
    assert!(shown.contains("took the suggestion back"), "{shown}");
    let back = &read_comments(&path).comments[0];
    assert_eq!(
        (back.applied.as_ref(), back.replaced.as_ref()),
        (None, None)
    );
    assert_eq!(
        back.anchor.as_ref().map(|a| &a.lines[..]),
        Some(&lines(&["line 1"])[..])
    );
    reload(&mut app, diff(FILE));

    apply(&mut app);
    // Discarded with `D`: the file has the lines it replaced again, so it
    // says it applies.
    reload(&mut app, diff(FILE));
    app.follow_thread(&root);
    app.message = None;
    app.dirty = true;
    let shown = status(&mut app);
    assert!(shown.contains("s apply"), "{shown}");
    assert_eq!(s(&mut app), (lines(&["line 1"]), lines(&["line one"])));
    let stale = &read_comments(&path).comments[0];
    assert_eq!(stale.applied, None, "no longer marked applied");
}

/// Runs `git` in `dir`, with no config of the user's.
fn git_in(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
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
}

/// A viewer of the working tree in `d`, which changed `changed.txt` and
/// not `kept.rs`, with the comments in `comments`: a thread with a
/// suggestion on `kept.rs`'s line 2, and one on `gone.rs`, which isn't
/// there. Its uncommitted changes list `changed.txt` alone.
fn beyond_the_diff(d: &Path, comments: &Path) -> (App, Repo, ChangeSet) {
    git_in(d, &["init", "-q", "-b", "main"]);
    std::fs::write(d.join("changed.txt"), "a\n").expect("write");
    std::fs::write(d.join("kept.rs"), "line 1\nline 2\nline 3\nline 4\n").expect("write");
    git_in(d, &["add", "-A"]);
    git_in(d, &["commit", "-q", "-m", "x"]);
    std::fs::write(d.join("changed.txt"), "b\n").expect("write");
    let at = |n: usize| {
        format!(r#""side":"new","start":{{"old":null,"new":{n}}},"end":{{"old":null,"new":{n}}}"#)
    };
    std::fs::write(
        comments,
        format!(
            r#"{{"version":1,"comments":[
{{"id":"k1","path":"kept.rs",{},"text":"Two?","suggestion":"line TWO","author":"","created":"2026-11-17T10:00:00Z","anchor":{{"before":["line 1"],"lines":["line 2"],"after":["line 3"]}}}},
{{"id":"g1","path":"gone.rs",{},"text":"Gone","author":"","created":"2026-11-17T10:00:00Z","anchor":{{"before":[],"lines":["gone"],"after":[]}}}}
]}}"#,
            at(2),
            at(1)
        ),
    )
    .expect("write");
    let repo = Repo::discover(d).expect("repo");
    let spec = repo.resolve(&[], false, false).expect("spec");
    let mut set = ChangeSet::new(repo.clone(), spec, Revisions::default(), &[], false);
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        None,
        (140, 16),
        Indicators::Classic,
    );
    set.enumerate(|change| {
        app.event(Event::Enumerated(change));
        ControlFlow::Continue(())
    })
    .expect("enumerate");
    app.event(Event::EnumerationDone(None));
    app.set_worktree(Some(d.to_owned()));
    app.set_comments(comments.to_owned());
    diff_shown(&mut app, &repo);
    screen(&mut app);
    (app, repo, set)
}

/// Draws, then runs the diffs the viewer asks for in the pool's worker.
fn diff_shown(app: &mut App, repo: &Repo) {
    app.draw();
    let settings = crate::tui::worker::Settings {
        options: Options::default(),
        highlight: false,
        grammars: crate::grammars(),
        cache: None,
    };
    let jobs: Vec<Job> = (app.jobs().into_iter())
        .filter(|job| matches!(job, Job::Diff { .. }))
        .collect();
    for job in jobs {
        let done = crate::tui::worker::run(job, Some(repo), &settings);
        app.event(Event::Diffed(done));
    }
}

/// `enter` on a thread whose file the diff doesn't list opens that file
/// after the others, whole and marked as having no changes, with the
/// thread focused on its line; the panel says up front which such file
/// isn't on disk. The file stays through a refresh of other files, and
/// its suggestion applies there. Once the file changes, the list has it,
/// and it's a file like any other.
#[test]
fn enter_opens_a_threads_file_the_diff_doesnt_list() {
    let (repo_dir, dir) = (
        tempfile::tempdir().expect("tempdir"),
        tempfile::tempdir().expect("tempdir"),
    );
    let d = repo_dir.path();
    let (mut app, repo, mut set) = beyond_the_diff(d, &dir.path().join("comments.json"));
    key(&mut app, 'i');
    let shown = screen(&mut app);
    assert!(shown.contains("kept.rs · enter opens the file"), "{shown}");
    assert!(shown.contains("gone.rs · file not on disk"), "{shown}");
    app.panel.as_mut().expect("the comments show").select("k1");
    press(&mut app, Key::Enter);
    let files = app.doc.document.files();
    assert_eq!(files.len(), 2);
    assert!(files[1].is_unchanged());
    assert_eq!(app.doc.document.pos.file, 1);
    diff_shown(&mut app, &repo);
    screen(&mut app);
    assert_eq!(focused(&app), Some("k1"));
    key(&mut app, 'i');
    app.message = None;
    insta::assert_snapshot!("unchanged_file", screen(&mut app));

    std::fs::write(d.join("changed.txt"), "c\n").expect("write");
    let update = set
        .refresh(&["changed.txt".into()])
        .expect("refresh")
        .expect("an update");
    app.event(Event::Refreshed(update));
    let files = app.doc.document.files();
    assert!(
        files[1].is_unchanged() && !files[1].wants_diff(),
        "kept as it was"
    );
    assert_eq!(app.doc.document.pos.file, 1);
    screen(&mut app);
    assert_eq!(focused(&app), Some("k1"));

    let Some(Action::ApplySuggestion(suggestion)) = press(&mut app, Key::Char('s')) else {
        panic!("applies: {}", status(&mut app));
    };
    crate::tui::apply(&repo, &suggestion).expect("applied");
    assert_eq!(
        std::fs::read_to_string(d.join("kept.rs")).expect("read"),
        "line 1\nline TWO\nline 3\nline 4\n"
    );
    let update = set.rescan().expect("rescan").expect("an update");
    app.event(Event::Refreshed(update));
    let files = app.doc.document.files();
    let paths: Vec<_> = files.iter().map(|e| e.change().path.clone()).collect();
    assert_eq!(paths, ["changed.txt", "kept.rs"]);
    assert!(!files[1].is_unchanged(), "the list's own entry");
}

/// `m` past the last thread goes on to the next file with threads that
/// the diff doesn't list, past one that isn't on disk, which `enter` in
/// the panel opens nothing for.
#[test]
fn m_goes_on_to_a_threads_file_the_diff_doesnt_list() {
    let (repo_dir, dir) = (
        tempfile::tempdir().expect("tempdir"),
        tempfile::tempdir().expect("tempdir"),
    );
    let (mut app, repo, _) = beyond_the_diff(repo_dir.path(), &dir.path().join("comments.json"));
    key(&mut app, 'm');
    let files = app.doc.document.files();
    assert_eq!(files.len(), 2);
    assert_eq!(files[1].change().path, "kept.rs");
    assert_eq!(app.doc.document.pos.file, 1);
    diff_shown(&mut app, &repo);
    screen(&mut app);
    assert_eq!(focused(&app), Some("k1"));

    key(&mut app, 'i');
    let shown = screen(&mut app);
    assert!(shown.contains("gone.rs · file not on disk"), "{shown}");
    app.panel.as_mut().expect("the comments show").select("g1");
    press(&mut app, Key::Enter);
    let shown = status(&mut app);
    assert!(shown.contains("gone.rs isn't on disk"), "{shown}");
    assert_eq!(app.doc.document.files().len(), 2);
    assert_eq!(app.pane, Pane::Tree, "the keys stay in the panel");
}

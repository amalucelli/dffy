use super::support::*;

/// A viewer of a piped diff, read whole.
fn piped_app(input: &[u8], width: usize, height: usize) -> App {
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "stdin".to_owned(),
        None,
        (width, height),
        Indicators::Classic,
    );
    app.set_piped();
    for item in piped::items(input) {
        app.event(Event::Piped(item));
    }
    app.event(Event::EnumerationDone(None));
    app
}

/// Does the workers' diffing of piped files, on this thread.
fn read_back(app: &mut App) {
    app.draw();
    for job in app.jobs() {
        if let Job::Diff {
            file,
            version,
            change,
            piped: Some(piped),
            ..
        } = job
        {
            let loaded = piped::read(&piped, change).expect("in memory");
            app.event(Event::Diffed(Done::Diff {
                placed: None,
                file,
                version,
                result: Ok(Box::new(loaded)),
            }));
        }
    }
}

const LOG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/git-log-color.diff"
));

/// `git log -p` in the viewer: each commit a top-level node of the tree
/// with its files beneath, its header and file list as its view, and
/// `n` going on from one commit's changes into the next's.
#[test]
fn a_piped_log_shows_commits_in_the_tree_and_their_headers() {
    let mut app = piped_app(LOG, 120, 24);
    read_back(&mut app);
    insta::assert_snapshot!("piped_log_commit", screen(&mut app));
    key(&mut app, 'n');
    read_back(&mut app);
    insta::assert_snapshot!("piped_log_file", screen(&mut app));
    key(&mut app, 'n');
    read_back(&mut app);
    let shown = status(&mut app);
    assert!(
        shown.contains(" commit 2/2 ") && shown.contains(" 1/7 · "),
        "{shown}"
    );
    assert!(
        shown.contains("→ commit 2/2: 80f116f Widen calc"),
        "{shown}"
    );
    // The unchanged lines between hunks aren't in the input.
    key(&mut app, 'x');
    assert!(
        screen(&mut app).contains("a piped diff has only its hunks"),
        "{}",
        screen(&mut app)
    );
    // Collapsing the first commit hides its file in the tree.
    app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
    key(&mut app, 'g');
    key(&mut app, 'h');
    let shown = screen(&mut app);
    let rows: Vec<&str> = shown.lines().collect();
    assert!(rows[1].contains("▶ 1fcf726 Add a third line"), "{shown}");
    assert!(rows[2].contains("▼ 80f116f Widen calc"), "{shown}");
}

/// Lines of a combined diff longer than the screen end in `…` and
/// scroll sideways like code.
#[test]
fn a_long_combined_line_is_cut_and_scrolls() {
    let long = format!("++{}END", "x".repeat(150));
    let input = format!(
        "diff --cc f.txt\nindex 1,2..3\n--- a/f.txt\n+++ b/f.txt\n@@@ -1,1 -1,1 +1,1 @@@\n- a\n -b\n{long}\n"
    );
    let mut app = piped_app(input.as_bytes(), 80, 12);
    let row = |app: &mut App| {
        screen(app)
            .lines()
            .find(|l| l.contains("xxx"))
            .map(str::to_owned)
            .unwrap_or_default()
    };
    let before = row(&mut app);
    assert!(before.ends_with('…') && !before.contains("END"), "{before}");
    for _ in 0..10 {
        key(&mut app, '>');
    }
    let after = row(&mut app);
    assert!(after.starts_with('…') && after.contains("END"), "{after}");
}

/// The screen's first `width` columns with each run of cells in one of
/// `roles`' styles in `<role>…</>`.
fn role_markup(app: &mut App, roles: &[Role], width: usize) -> String {
    let styles: Vec<_> = (roles.iter())
        .map(|&r| (app.theme.style(r), r.key().to_owned()))
        .collect();
    style_markup(app, &styles, width)
}

/// `git show` of a merge in the viewer: the commit's header, metadata
/// and message in their colors, then the combined diff's lines colored
/// by their prefix columns.
#[test]
fn a_piped_merge_shows_its_commit_and_combined_diff_in_their_colors() {
    let merge = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/git-show-merge.diff"
    ));
    let roles = [
        Role::CommitHeader,
        Role::Dim,
        Role::HunkHeader,
        Role::Added,
        Role::Removed,
    ];
    let mut app = piped_app(merge, 100, 14);
    // The default theme shares colors between these roles and others
    // (`warn` is yellow too): underlined, each is told apart.
    app.theme = Theme::parse(
        "roles",
        "commit-header = yellow underline\ndim = dim underline\n\
             hunk-header = cyan underline\nadded = green underline\nremoved = red underline\n",
        &mut Vec::new(),
    );
    key(&mut app, 't');
    read_back(&mut app);
    insta::assert_snapshot!(
        "piped_merge_commit",
        role_markup(&mut app, &roles, usize::MAX)
    );
    key(&mut app, 'f');
    read_back(&mut app);
    insta::assert_snapshot!(
        "piped_merge_combined",
        role_markup(&mut app, &roles, usize::MAX)
    );
}

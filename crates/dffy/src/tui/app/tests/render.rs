use super::support::*;

/// In the rendered view the title counts what the tree counts: the
/// file's changed source lines, not its changed rendered lines.
#[test]
fn the_rendered_view_titles_the_trees_counts() {
    let side = Some(Side::Worktree {
        mode: Mode::File,
        id: None,
    });
    let old = Text::new("Intro.\n\nGone para here.\n\nBeta three four.\n\nTail.\n".into());
    let new = Text::new("Intro.\n\nBeta three FOUR.\n\nTail.\n".into());
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
    let mut app = app_of(&config, "working tree", fd, (120, 20));
    let para = |source: usize, text: &str| plugin::Block {
        kind: "paragraph".to_owned(),
        source: source..source + 1,
        lines: [text, ""]
            .map(|text| plugin::Line {
                text: text.to_owned(),
                paint: Vec::new(),
                links: Vec::new(),
            })
            .to_vec(),
        language: None,
    };
    let sides = [
        plugin::Side {
            blocks: vec![
                para(0, "Intro."),
                para(2, "Gone para here."),
                para(4, "Beta three four."),
                para(6, "Tail."),
            ],
        },
        plugin::Side {
            blocks: vec![
                para(0, "Intro."),
                para(2, "Beta three FOUR."),
                para(4, "Tail."),
            ],
        },
    ];
    let diff = Arc::clone(app.doc.document.diff_of(0).expect("diffed"));
    let rendered = view::rendered::build(Arc::clone(&diff), sides, crate::grammars(), 3, 60);
    assert_eq!(
        rendered.counts(),
        Counts {
            added: 1,
            removed: 3
        }
    );
    app.event(Event::Diffed(Done::Rendered {
        file: 0,
        version: app.version(0),
        id: app.render_id,
        diff,
        result: Ok(Box::new(rendered)),
    }));
    app.doc.set_rendered_mode(true);
    let shown = screen(&mut app);
    assert!(shown.contains("Beta three FOUR."), "{shown}");
    let title = shown.lines().next().expect("the title row");
    let tree = shown
        .lines()
        .find(|l| l.contains("README.md") && l.contains('│'))
        .expect("the tree's row");
    assert!(title.contains("+1 -3"), "{title}");
    assert!(tree.contains("+1") && tree.contains("-3"), "{tree}");
}

/// A `render` setting alone looks nothing up: drawing and moving
/// between files of the renderer's type ask for no program, and no
/// render job is queued, until `r` asks for the rendered view.
#[test]
fn renderers_are_looked_up_only_when_r_asks() {
    let mut config = Config::default();
    config.set("render", "rs:nothere").expect("valid");
    let mut app = app_of(&config, "working tree", diff("src/a.rs"), (120, 20));
    for c in ['f', 'F', 'n', 'N', 'j'] {
        screen(&mut app);
        key(&mut app, c);
    }
    screen(&mut app);
    assert_eq!(app.programs.len(), 0);
    assert!(!app.jobs().iter().any(|j| matches!(j, Job::Render(_))));
    key(&mut app, 'r');
    assert_eq!(app.programs.len(), 1, "looked up once r asked");
    assert!(!app.doc.document.rendered(), "not on without the program");
}

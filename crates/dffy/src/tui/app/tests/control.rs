use super::support::*;
use crate::control::viewer::{Command, Inbound, Reply};

/// A viewer of two files whose sides are a commit and the working tree.
fn sided() -> App {
    let mut app = app(120, 24);
    let hex = "ab".repeat(20);
    let tree = dffy_core::git::ObjectId::from_hex(hex.as_bytes()).expect("hex");
    app.set_snapshots(Some([Snapshot::Tree(tree), Snapshot::Worktree]));
    app
}

/// `view/open` of `path` as the control API asks it: what the viewer
/// hands the loop, and the answer when it came at once.
fn open(
    app: &mut App,
    path: &str,
    line: Option<usize>,
    side: document::Side,
) -> (Option<Action>, Answer) {
    let (reply, answer) = Reply::channel();
    let command = Command::Open {
        path: path.to_owned(),
        line,
        side,
    };
    let action = app.event(Event::Control(Inbound::Command(command, reply)));
    (action, Answer(answer))
}

struct Answer(std::sync::mpsc::Receiver<String>);

impl Answer {
    fn now(&self) -> Option<String> {
        self.0.try_recv().ok()
    }
}

const ON_DISK: Side = Side::Worktree {
    mode: Mode::File,
    id: None,
};

/// `text` compared with itself, as the workers read a file opened outside
/// the diff.
fn whole(path: &str, text: &str) -> FileDiff {
    let text = Text::new(text.as_bytes().to_vec());
    FileDiff {
        change: FileChange::unchanged(path.into(), ON_DISK),
        hunks: Vec::new(),
        old: text.clone(),
        new: text,
        binary: false,
        too_large: false,
        note: None,
    }
}

/// A path the diff doesn't list is looked up at the side asked for and
/// opened whole there; the answer says it isn't listed. A binary or too
/// large one opens, but has no lines to go to.
#[test]
fn view_open_opens_a_file_outside_the_diff_whole() {
    let mut app = sided();
    let (action, answer) = open(&mut app, "notes.txt", Some(39), document::Side::Old);
    assert_eq!(
        action,
        Some(Action::OpenOther {
            path: "notes.txt".into(),
            at: app.snapshot(document::Side::Old).expect("a snapshot"),
            side: document::Side::Old,
        })
    );
    assert_eq!(answer.now(), None, "not until it's found and read");
    app.other_found("notes.txt".into(), document::Side::Old, Ok(Some(ON_DISK)));
    assert_eq!(answer.now(), None, "not until it's read");
    let file = app.doc.document.pos.file;
    assert!(app.doc.document.files()[file].is_unchanged());
    let text = (1..=60).map(|i| format!("note {i}\n")).collect::<Vec<_>>();
    let text = text.concat();
    let done = diffed(&app, file, whole("notes.txt", &text));
    app.event(Event::Diffed(done));
    assert_eq!(
        answer.now().as_deref(),
        Some(concat!(
            r#"{"id":1,"result":{"path":"notes.txt","line":40,"side":"old","listed":false}}"#,
            "\n"
        ))
    );
    assert!(screen(&mut app).contains("note 40"));

    // Neither has text to read.
    for (fd, why) in [
        (
            FileDiff {
                binary: true,
                ..whole("logo.bin", "")
            },
            "logo.bin is binary; no lines",
        ),
        (
            FileDiff {
                too_large: true,
                ..whole("huge.log", "")
            },
            "huge.log is too large to show; no lines",
        ),
    ] {
        let path = fd.change.path.to_string();
        let (action, answer) = open(&mut app, &path, Some(0), document::Side::New);
        assert!(
            matches!(action, Some(Action::OpenOther { .. })),
            "{action:?}"
        );
        app.other_found(path.as_str().into(), document::Side::New, Ok(Some(ON_DISK)));
        let file = app.doc.document.pos.file;
        let done = diffed(&app, file, fd);
        app.event(Event::Diffed(done));
        let answer = answer.now().unwrap_or_default();
        assert!(
            answer.contains(r#""code":"not_found""#) && answer.contains(why),
            "{answer}"
        );
    }
}

/// `comments/add` on a path the diff doesn't list opens it as `view/open`
/// does, then adds the comment there; the answer says it isn't listed.
/// No file there is `not_found`, and nothing is written.
#[test]
fn comments_add_opens_a_file_outside_the_diff_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = sided();
    app.set_comments(path.clone());
    let add = |app: &mut App, path: &str| {
        let (reply, answer) = Reply::channel();
        let command = Command::Comment {
            path: path.to_owned(),
            line: 2,
            side: document::Side::New,
            text: "Why kept?".to_owned(),
            author: "claude".to_owned(),
        };
        let action = app.event(Event::Control(Inbound::Command(command, reply)));
        (action, Answer(answer))
    };

    let (action, answer) = add(&mut app, "src");
    assert!(
        matches!(action, Some(Action::OpenOther { .. })),
        "{action:?}"
    );
    app.other_found("src".into(), document::Side::New, Ok(None));
    let missing = answer.now().unwrap_or_default();
    assert!(
        missing.contains("src isn't in this diff, nor at its new side"),
        "{missing}"
    );
    assert!(!path.exists(), "nothing written");

    let (action, answer) = add(&mut app, "notes.txt");
    assert!(
        matches!(action, Some(Action::OpenOther { .. })),
        "{action:?}"
    );
    app.other_found("notes.txt".into(), document::Side::New, Ok(Some(ON_DISK)));
    assert_eq!(answer.now(), None, "not until it's read");
    let file = app.doc.document.pos.file;
    let text = (1..=9).map(|i| format!("note {i}\n")).collect::<Vec<_>>();
    let done = diffed(&app, file, whole("notes.txt", &text.concat()));
    app.event(Event::Diffed(done));
    let answer = answer.now().unwrap_or_default();
    assert!(
        answer.contains(r#""path":"notes.txt","line":3,"side":"new","listed":false"#),
        "{answer}"
    );
    let written = std::fs::read_to_string(&path).expect("written");
    assert!(
        written.contains(r#""path":"notes.txt""#) && written.contains("Why kept?"),
        "{written}"
    );
}

/// A comment added at the older version of a file the diff doesn't list
/// is on that version's line, and its thread shows on the file opened
/// there.
#[test]
fn comments_add_at_the_older_version_places_its_thread() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = sided();
    app.set_comments(path.clone());
    let (reply, answer) = Reply::channel();
    let command = Command::Comment {
        path: "notes.txt".to_owned(),
        line: 4,
        side: document::Side::Old,
        text: "Why kept?".to_owned(),
        author: "claude".to_owned(),
    };
    let action = app.event(Event::Control(Inbound::Command(command, reply)));
    assert!(
        matches!(
            action,
            Some(Action::OpenOther {
                side: document::Side::Old,
                ..
            })
        ),
        "{action:?}"
    );
    app.other_found("notes.txt".into(), document::Side::Old, Ok(Some(ON_DISK)));
    let file = app.doc.document.pos.file;
    let text = (1..=9).map(|i| format!("note {i}\n")).collect::<Vec<_>>();
    let done = diffed(&app, file, whole("notes.txt", &text.concat()));
    app.event(Event::Diffed(done));
    let answer = Answer(answer).now().unwrap_or_default();
    assert!(
        answer.contains(r#""line":5,"side":"old","listed":false"#),
        "{answer}"
    );
    let written = std::fs::read_to_string(&path).expect("written");
    assert!(written.contains(r#""side":"old""#), "{written}");
    let shown = screen(&mut app);
    assert!(
        shown.contains("Why kept?") && shown.contains("note 5"),
        "{shown}"
    );
}

/// A request waiting for a file opened outside the diff fails when the
/// file is opened at the other version before it shows: its line would be
/// read on the wrong one.
#[test]
fn a_waiting_request_fails_once_its_file_is_opened_at_the_other_version() {
    let mut app = sided();
    let (action, answer) = open(&mut app, "notes.txt", Some(3), document::Side::New);
    assert!(action.is_some());
    app.other_found("notes.txt".into(), document::Side::New, Ok(Some(ON_DISK)));
    assert_eq!(answer.now(), None, "waiting for the file");
    app.other_found("notes.txt".into(), document::Side::Old, Ok(Some(ON_DISK)));
    let file = app.doc.document.pos.file;
    let text = (1..=9).map(|i| format!("note {i}\n")).collect::<Vec<_>>();
    let done = diffed(&app, file, whole("notes.txt", &text.concat()));
    app.event(Event::Diffed(done));
    let failed = answer.now().unwrap_or_default();
    let old = app.version_name(document::Side::Old);
    assert!(
        failed.contains(r#""code":"failed""#)
            && failed.contains(&format!("notes.txt was opened at {old} before it showed")),
        "{failed}"
    );
}

/// No file at the side asked for, as for a folder, a gitlink or a path
/// nowhere, is `not_found`, said to the asker and not on the viewer's
/// status line, the view left as it was; a lookup that fails is `failed`.
#[test]
fn view_open_of_no_file_there_is_not_found() {
    let mut app = sided();
    app.draw();
    key(&mut app, 'v');
    assert!(app.doc.selection.is_some());
    let (action, answer) = open(&mut app, "src", None, document::Side::New);
    assert!(action.is_some());
    app.other_found("src".into(), document::Side::New, Ok(None));
    let answer = answer.now().unwrap_or_default();
    assert!(
        answer.contains(r#""code":"not_found""#)
            && answer.contains("src isn't in this diff, nor at its new side"),
        "{answer}"
    );
    assert!(app.message.is_none(), "{:?}", app.message);
    assert!(app.doc.selection.is_some(), "the view stays as it was");

    let (_, answer) = open(&mut app, "notes.txt", None, document::Side::New);
    app.other_found(
        "notes.txt".into(),
        document::Side::New,
        Err("unreadable".to_owned()),
    );
    let answer = answer.now().unwrap_or_default();
    assert!(
        answer.contains(r#""code":"failed""#)
            && answer.contains("couldn't open notes.txt: unreadable"),
        "{answer}"
    );
}

/// While the diff's files are still being read, neither `view/open` nor
/// `comments/add` opens a path not among them yet: it may be one still to
/// come, which would then be listed twice. Nothing is written.
#[test]
fn nothing_outside_the_diff_opens_while_its_files_are_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let mut app = App::new(
        &Config::default(),
        Theme::default_theme(),
        "working tree".to_owned(),
        None,
        (120, 24),
        Indicators::Classic,
    );
    app.set_snapshots(sided().snapshots);
    app.set_comments(path.clone());
    let (reply, comment) = Reply::channel();
    let add = Command::Comment {
        path: "notes.txt".to_owned(),
        line: 0,
        side: document::Side::New,
        text: "Why kept?".to_owned(),
        author: "claude".to_owned(),
    };
    let added = app.event(Event::Control(Inbound::Command(add, reply)));
    let (opened, open) = open(&mut app, "notes.txt", None, document::Side::New);
    for (action, answer) in [(added, Answer(comment)), (opened, open)] {
        assert_eq!(action, None);
        let answer = answer.now().unwrap_or_default();
        assert!(
            answer.contains(r#""code":"not_found""#)
                && answer.contains("notes.txt isn't in this diff; still reading the diff's files"),
            "{answer}"
        );
    }
    assert!(!path.exists(), "nothing written");
}

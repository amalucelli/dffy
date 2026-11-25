use std::sync::Weak;
use std::sync::atomic::{AtomicBool, Ordering};

use super::support::*;
use crate::control::viewer::{Command, Facts, Inbound, Listens, Reply};
use crate::review::model::text::now;

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

/// A session that listens, as dffyd hands its request over.
struct Session {
    answer: Answer,
    /// dffyd calls its request off through this.
    cancelled: Weak<AtomicBool>,
}

/// The session numbered `number`, called `name`, asks, having started,
/// edited and been prompted so many seconds ago.
fn listens(
    app: &mut App,
    (number, name): (u64, &str),
    [started, edited, prompted]: [Option<u64>; 3],
) -> Session {
    let (reply, answer) = Reply::channel();
    let cancelled = reply.watch();
    let at = |ago: Option<u64>| ago.map(|s| now() - s);
    let listens = Listens {
        request: number + 100,
        listener: number,
        client: Some(name.to_owned()),
        facts: Facts {
            started: at(started).unwrap_or_else(now),
            edited: at(edited),
            prompted: at(prompted),
        },
    };
    app.event(Event::Control(Inbound::Listens(listens, reply)));
    Session {
        answer: Answer(answer),
        cancelled,
    }
}

const IDLE: [Option<u64>; 3] = [None; 3];

/// dffyd calls `session`'s request off.
fn call_off(app: &mut App, session: &Session) {
    if let Some(flag) = session.cancelled.upgrade() {
        flag.store(true, Ordering::Relaxed);
    }
    app.event(Event::Control(Inbound::Cancelled));
}

/// `f` on the focused thread, as far as the loop: the name of the session
/// it goes to, `None` for a new agent or nothing yet.
fn sends(app: &mut App) -> Option<String> {
    key(app, 'f');
    hands_over(app)
}

/// The loop takes what's handed over.
fn hands_over(app: &mut App) -> Option<String> {
    let handoff = app.take_handoff()?;
    match app.hand_over(&handoff) {
        HandTo::Session(
            reply,
            Listening {
                name, told_apart, ..
            },
        ) => {
            reply.send(Ok(crate::control::object(vec![(
                "prompt",
                crate::control::text("Address it"),
            )])));
            app.event(Event::HandedOff {
                started: Ok(format!("sent the thread to {name}{told_apart}")),
                left: None,
                unmarked: None,
            });
            Some(name)
        }
        HandTo::Agent => Some("an agent".to_owned()),
        HandTo::Later => None,
    }
}

/// [`commented`] with its thread `d1` focused, at 120 columns.
fn on_thread(dir: &tempfile::TempDir) -> App {
    let (mut app, _) = commented(dir);
    app.resize(120, 24);
    app.follow_thread("d1");
    app
}

/// A session's request is held however long: it's neither refused nor
/// waited on while a comment is written, and wakes nothing.
#[test]
fn a_listening_session_is_held_without_waiting_on_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    key(&mut app, 'c');
    assert!(app.doc.writing().is_some());
    app.message = None;
    let claude = listens(&mut app, (1, "claude"), IDLE);
    assert_eq!(claude.answer.now(), None);
    assert_eq!(app.deadline(), None);
    esc(&mut app);
    app.follow_thread("d1");
    assert_eq!(sends(&mut app).as_deref(), Some("claude"));
    assert_eq!(
        claude.answer.now().as_deref(),
        Some("{\"id\":1,\"result\":{\"prompt\":\"Address it\"}}\n")
    );
    assert!(status(&mut app).contains("sent the thread to claude"));
}

/// With several sessions, the only one that edited gets the threads, as it
/// says later too; without sessions, a new agent does.
#[test]
fn a_hand_over_goes_to_the_only_session_that_edited() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    assert_eq!(sends(&mut app).as_deref(), Some("an agent"));
    let _claude = listens(&mut app, (1, "claude"), IDLE);
    let _codex = listens(&mut app, (2, "codex"), IDLE);
    let facts = Facts {
        started: now(),
        edited: Some(now()),
        prompted: None,
    };
    let request = 101;
    app.event(Event::Control(Inbound::Listened { request, facts }));
    assert_eq!(sends(&mut app).as_deref(), Some("claude"));
}

/// Otherwise they're listed, the last to edit first, then the last
/// prompted, then the newest. `esc` sends nothing; `enter` sends to the
/// row it's on, and every hand-over after goes there too, until that
/// session leaves.
#[test]
fn unclear_sessions_are_picked_from_and_the_pick_sticks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    let hour = Some(3600);
    let _a = listens(&mut app, (1, "amp"), [Some(7200), None, None]);
    let _b = listens(&mut app, (2, "pi"), [hour, None, None]);
    let _c = listens(&mut app, (3, "codex"), [hour, None, Some(300)]);
    let _d = listens(&mut app, (4, "claude"), [Some(7200), Some(180), None]);
    let e = listens(&mut app, (5, "x\u{1b}[31m"), [hour, Some(60), None]);
    assert_eq!(sends(&mut app), None);
    let shown = screen(&mut app);
    let rows: Vec<&str> = (shown.lines())
        .flat_map(|l| l.split('│'))
        .filter(|l| l.contains(" · started "))
        .map(str::trim)
        .collect();
    assert_eq!(
        rows,
        [
            "x^[[31m · started 1h ago · edited 1m ago",
            "claude · started 2h ago · edited 3m ago",
            "codex · started 1h ago · prompted 5m ago",
            "pi · started 1h ago",
            "amp · started 2h ago",
        ]
    );
    esc(&mut app);
    assert_eq!(hands_over(&mut app), None);
    assert!(status(&mut app).contains("nothing sent"));

    assert_eq!(sends(&mut app), None);
    key(&mut app, 'j');
    // The list stays as it is under the cursor when one leaves.
    call_off(&mut app, &e);
    std::thread::sleep(Duration::from_millis(1050));
    app.event(Event::Tick);
    press(&mut app, Key::Enter);
    assert_eq!(hands_over(&mut app).as_deref(), Some("claude"));
    // It asks again, and another edits after it.
    let claude = listens(&mut app, (4, "claude"), [Some(7200), Some(180), None]);
    let _e = listens(&mut app, (6, "x"), [Some(0), Some(0), None]);
    assert_eq!(sends(&mut app).as_deref(), Some("claude"));
    call_off(&mut app, &claude);
    std::thread::sleep(Duration::from_millis(1050));
    assert_eq!(sends(&mut app).as_deref(), Some("x"), "once it left");
}

/// Between a hand-over and asking again, a session still counts: what's
/// sent to it meanwhile waits for it, and starts no agent. One that doesn't
/// ask again gets nothing, which is said.
#[test]
fn a_hand_over_waits_for_a_session_about_to_ask_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    let _claude = listens(&mut app, (1, "claude"), IDLE);
    let _codex = listens(&mut app, (2, "codex"), [None, None, Some(9)]);
    assert_eq!(sends(&mut app), None);
    key(&mut app, 'j');
    press(&mut app, Key::Enter);
    assert_eq!(hands_over(&mut app).as_deref(), Some("claude"));
    assert_eq!(sends(&mut app), None, "codex isn't the one picked");
    assert!(app.listeners.picking.is_none());
    assert!(status(&mut app).contains("waiting for claude…"));
    let _claude = listens(&mut app, (1, "claude"), IDLE);
    assert_eq!(hands_over(&mut app).as_deref(), Some("claude"));

    assert_eq!(sends(&mut app), None);
    std::thread::sleep(Duration::from_millis(1050));
    assert!(app.deadline().is_some_and(|at| at <= Instant::now()));
    app.event(Event::Tick);
    assert!(app.take_handoff().is_none());
    assert!(
        status(&mut app).contains("claude stopped listening; send again"),
        "{}",
        status(&mut app)
    );
}

/// A hand-over whose session another view called away during its checkpoint
/// goes to that session once it asks again, not to another that does; one
/// whose session left goes nowhere, which is said.
#[test]
fn a_hand_over_handed_back_waits_for_its_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    let claude = listens(&mut app, (1, "claude"), [None, Some(9), None]);
    let _codex = listens(&mut app, (2, "codex"), IDLE);
    let hand_back = |app: &mut App, claude: &Session| {
        key(app, 'f');
        let handoff = app.take_handoff().expect("handed");
        let HandTo::Session(reply, session) = app.hand_over(&handoff) else {
            panic!("not to a session");
        };
        call_off(app, claude);
        drop(reply);
        let back = Handoff {
            checkpointed: true,
            ..handoff.to(&session)
        };
        app.event(Event::HandedBack(Box::new(back)));
        assert!(status(app).contains("waiting for claude…"));
    };
    hand_back(&mut app, &claude);
    // Both edited by now, yet it's still claude's.
    let _codex = listens(&mut app, (2, "codex"), [None, Some(1), None]);
    assert!(app.take_handoff().is_none());
    let claude = listens(&mut app, (1, "claude"), [None, Some(9), None]);
    let handoff = app.take_handoff().expect("goes");
    assert!(handoff.checkpointed);
    let HandTo::Session(_, session) = app.hand_over(&handoff) else {
        panic!("not to a session");
    };
    assert_eq!(session.name, "claude");

    call_off(&mut app, &claude);
    app.event(Event::HandedBack(Box::new(handoff.to(&session))));
    std::thread::sleep(Duration::from_millis(1050));
    app.event(Event::Tick);
    assert!(app.take_handoff().is_none());
    assert!(status(&mut app).contains("claude stopped listening; send again"));
}

/// When dffyd goes, so do the sessions and whom was picked: what waited for
/// one isn't sent, which is said.
#[test]
fn sessions_go_with_dffyd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    let _claude = listens(&mut app, (1, "claude"), IDLE);
    assert_eq!(sends(&mut app).as_deref(), Some("claude"));
    assert_eq!(sends(&mut app), None);
    app.event(Event::Control(Inbound::Gone));
    assert!(status(&mut app).contains("claude stopped listening; send again"));
    assert_eq!(sends(&mut app).as_deref(), Some("an agent"));
}

/// A hand-over the loop hasn't taken yet isn't lost to one that waited.
#[test]
fn a_hand_over_waiting_for_a_session_leaves_the_next_one_be() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    let _claude = listens(&mut app, (1, "claude"), IDLE);
    assert_eq!(sends(&mut app).as_deref(), Some("claude"));
    assert_eq!(sends(&mut app), None);
    app.follow_thread("f1");
    key(&mut app, 'f');
    let _claude = listens(&mut app, (1, "claude"), IDLE);
    let next = app.take_handoff().map(|h| h.what);
    assert_eq!(next, Some(Handed::Thread("f1".to_owned())));
    app.event(Event::Tick);
    let waited = app.take_handoff().map(|h| h.what);
    assert_eq!(waited, Some(Handed::Thread("d1".to_owned())));
}

/// Of several sessions, the one threads go to is told apart by what it last
/// did, in the panel, the menu and the note after; one alone isn't.
#[test]
fn one_of_several_sessions_is_named_by_what_it_last_did() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    app.resize(160, 30);
    let _one = listens(&mut app, (1, "claude"), [Some(7200), Some(10800), None]);
    key(&mut app, ' ');
    assert!(!screen(&mut app).contains("Send to claude ·"));
    let _other = listens(&mut app, (2, "claude"), [Some(7200), None, Some(60)]);
    let shown = screen(&mut app);
    assert!(
        shown.contains("f  Send to claude · edited 3h ago")
            && shown.contains("Send open threads to claude · edited 3h ago"),
        "{shown}"
    );
    esc(&mut app);
    assert_eq!(sends(&mut app).as_deref(), Some("claude"));
    assert!(
        status(&mut app).contains("sent the thread to claude · edited 3h ago"),
        "{}",
        status(&mut app)
    );
}

/// The focused thread's key and the note's box say it's sent, not fixed,
/// while a session listens.
#[test]
fn a_thread_is_sent_rather_than_fixed_while_a_session_listens() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    app.message = None;
    let hint = |app: &mut App| note(app).unwrap_or_default();
    assert!(hint(&mut app).contains("f fix"), "{}", hint(&mut app));
    let claude = listens(&mut app, (1, "claude"), IDLE);
    assert!(hint(&mut app).contains("f send"), "{}", hint(&mut app));
    assert!(app.dirty);
    call_off(&mut app, &claude);
    std::thread::sleep(Duration::from_millis(1050));
    app.event(Event::Tick);
    assert!(hint(&mut app).contains("f fix"), "{}", hint(&mut app));

    let _claude = listens(&mut app, (1, "claude"), IDLE);
    esc(&mut app);
    key(&mut app, 'v');
    key(&mut app, ' ');
    key(&mut app, '&');
    let shown = screen(&mut app);
    assert!(
        shown.contains("▾ you · for claude") && !shown.contains("fix with agent"),
        "{shown}"
    );
}

/// A menu that's open says where its entries go now.
#[test]
fn an_open_menu_follows_the_sessions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    right_click(&mut app, "Why two?");
    assert!(menu_rows(&app).contains(&"Fix with agent f".to_owned()));
    let claude = listens(&mut app, (1, "claude"), IDLE);
    assert!(menu_rows(&app).contains(&"Send to claude f".to_owned()));
    call_off(&mut app, &claude);
    std::thread::sleep(Duration::from_millis(1050));
    app.event(Event::Tick);
    assert!(menu_rows(&app).contains(&"Fix with agent f".to_owned()));
}

/// A tap on a session's row sends to it; one elsewhere on the list does
/// nothing.
#[test]
fn a_tap_picks_a_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    let _claude = listens(&mut app, (1, "claude"), IDLE);
    let _codex = listens(&mut app, (2, "codex"), IDLE);
    assert_eq!(sends(&mut app), None);
    let tap = |app: &mut App, text: &str| {
        let (x, y) = cell_of(app, text);
        app.event(Event::Input(Input::Click { x, y }));
        hands_over(app)
    };
    assert_eq!(tap(&mut app, "enter sends"), None);
    assert!(app.listeners.picking.is_some());
    assert_eq!(tap(&mut app, "claude · started").as_deref(), Some("claude"));
}

/// `h` asks before it sends every open thread of yours: `enter` sends to
/// the session it names, anything else keeps them and does nothing else.
#[test]
fn h_asks_before_it_sends_every_open_thread() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    app.resize(160, 30);
    esc(&mut app);
    let _claude = listens(&mut app, (1, "claude"), IDLE);
    let question = "send 4 threads to claude? enter sends · esc keeps";
    let top = app.doc.document.pos;
    for other in [
        Input::Key(Key::Esc, Mods::NONE),
        Input::Key(Key::Char('j'), Mods::NONE),
        Input::Key(Key::Char('y'), Mods::NONE),
        Input::Key(Key::Char('h'), Mods::NONE),
        Input::Paste("\r".to_owned()),
        Input::Click { x: 60, y: 5 },
    ] {
        key(&mut app, 'h');
        assert!(status(&mut app).contains(question), "{}", status(&mut app));
        app.event(Event::Input(other.clone()));
        assert!(app.take_handoff().is_none(), "{other:?}");
        assert!(status(&mut app).contains("nothing sent"), "{other:?}");
        assert_eq!(app.doc.document.pos, top, "{other:?}");
    }
    key(&mut app, 'h');
    press(&mut app, Key::Enter);
    assert!(app.take_handoff().is_none(), "before its question showed");

    key(&mut app, 'h');
    screen(&mut app);
    press(&mut app, Key::Enter);
    assert_eq!(hands_over(&mut app).as_deref(), Some("claude"));
}

/// The target and the count are read again at `enter`: another target, or
/// none any more, is asked about again.
#[test]
fn h_asks_again_when_its_target_changed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    app.resize(160, 30);
    esc(&mut app);
    let ask = |app: &mut App| {
        key(app, 'h');
        status(app)
    };
    let enter = |app: &mut App| {
        press(app, Key::Enter);
        let handed = app.take_handoff().is_some();
        (handed, status(app))
    };
    assert!(ask(&mut app).contains("start your agent on 4 threads? enter starts · esc keeps"));
    let claude = listens(&mut app, (1, "claude"), IDLE);
    let (handed, shown) = enter(&mut app);
    assert!(
        !handed && shown.contains("send 4 threads to claude? enter sends"),
        "{shown}"
    );
    call_off(&mut app, &claude);
    std::thread::sleep(Duration::from_millis(1050));
    app.event(Event::Tick);
    let (handed, shown) = enter(&mut app);
    assert!(
        !handed && shown.contains("start your agent on 4 threads?"),
        "{shown}"
    );
    key(&mut app, ' ');
    let path = dir.path().join("comments.json");
    let op = Op::Set {
        target: Target::Id("f1".to_owned()),
        change: Box::new(Change {
            resolved: Some(true),
            ..Change::default()
        }),
    };
    assert!(ask(&mut app).contains("on 4 threads?"));
    review::apply(&path, vec![op]).expect("applies");
    app.event(Event::Comments);
    let (handed, shown) = enter(&mut app);
    assert!(
        !handed && shown.contains("start your agent on 3 threads?"),
        "{shown}"
    );
    press(&mut app, Key::Enter);
    assert_eq!(hands_over(&mut app).as_deref(), Some("an agent"));
}

/// When which session isn't clear, `h` goes to the list to pick from, which
/// is its question; with nothing to send, it says so and asks nothing.
#[test]
fn h_asks_nothing_twice_and_nothing_without_threads() {
    let mut bare = app(100, 20);
    key(&mut bare, 'h');
    assert!(status(&mut bare).contains("no open threads of yours to send"));
    assert!(bare.confirm.is_none());

    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    esc(&mut app);
    let _claude = listens(&mut app, (1, "claude"), IDLE);
    let _codex = listens(&mut app, (2, "codex"), IDLE);
    key(&mut app, 'h');
    assert!(app.confirm.is_none());
    assert_eq!(hands_over(&mut app), None);
    press(&mut app, Key::Enter);
    assert_eq!(hands_over(&mut app).as_deref(), Some("codex"));
}

/// `h` leaves lines selected as they are, asks on a focused thread too, and
/// stays the tree's collapse and the panel's step; `H` still scrolls.
#[test]
fn h_means_what_it_did_elsewhere() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    let asked = |app: &mut App| app.confirm.take().is_some();
    key(&mut app, 'h');
    assert!(asked(&mut app), "on a thread");
    esc(&mut app);
    key(&mut app, 'v');
    key(&mut app, 'h');
    assert!(asked(&mut app) && app.doc.selection.is_some());
    esc(&mut app);
    key(&mut app, 'H');
    assert!(!asked(&mut app));
    key(&mut app, ' ');
    key(&mut app, 'h');
    assert!(!asked(&mut app) && app.commands.is_some(), "the panel's");
    esc(&mut app);
    press(&mut app, Key::Tab);
    key(&mut app, 'h');
    assert!(!asked(&mut app), "the tree's");
    key(&mut app, 'i');
    key(&mut app, 'h');
    assert!(!asked(&mut app), "the comments panel's");
}

/// The command panel opens with no row to run, so `space` then `enter`
/// sends nothing; with no thread focused, its first row sends your open
/// threads, at once.
#[test]
fn the_panels_first_row_sends_the_open_threads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    esc(&mut app);
    key(&mut app, ' ');
    press(&mut app, Key::Enter);
    assert!(app.take_handoff().is_none() && app.commands.is_some());
    key(&mut app, 'j');
    press(&mut app, Key::Enter);
    let handed = app.take_handoff().map(|h| h.what);
    assert_eq!(handed, Some(Handed::Open { count: 4, left: 0 }));
}

/// The command panel, as it's open, and the menu say where both actions
/// send threads.
#[test]
fn the_panel_and_the_menu_say_where_threads_go() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = on_thread(&dir);
    key(&mut app, ' ');
    let shown = screen(&mut app);
    assert!(
        shown.contains("Fix with agent") && shown.contains("Send open threads to agent"),
        "{shown}"
    );
    let claude = listens(&mut app, (1, "x\u{1b}[31m"), IDLE);
    let shown = screen(&mut app);
    assert!(
        shown.contains("f  Send to x^[[31m") && shown.contains("Send open threads to x^[[31m"),
        "{shown}"
    );
    let _codex = listens(&mut app, (2, "codex"), IDLE);
    assert!(screen(&mut app).contains("f  Send to a session"));
    esc(&mut app);
    right_click(&mut app, "Why two?");
    assert!(menu_rows(&app).contains(&"Send to a session f".to_owned()));
    esc(&mut app);
    call_off(&mut app, &claude);
    std::thread::sleep(Duration::from_millis(1050));
    app.event(Event::Tick);
    key(&mut app, ' ');
    assert!(screen(&mut app).contains("f  Send to codex"));
    esc(&mut app);
    esc(&mut app);
    key(&mut app, 'v');
    key(&mut app, ' ');
    key(&mut app, '&');
    assert!(status(&mut app).contains("enter sends it to codex"));
}

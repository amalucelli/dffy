use super::support::*;

/// The diff's `O` and `R` are the command panel's: open a review from the
/// list, and the range since your last review.
#[test]
fn the_diffs_review_keys_are_the_panels() {
    let mut app = app(100, 20);
    app.reviewing.plugin = Some("test".to_owned());
    let (_, request) = plugin_run(press(&mut app, Key::Char('O')));
    assert_eq!(request, Request::List);
    assert_eq!(press(&mut app, Key::Char('R')), None);
    assert!(
        status(&mut app).contains("not on a review"),
        "{}",
        status(&mut app)
    );
}

/// The request asked for, with its id.
fn plugin_run(action: Option<Action>) -> (u64, Request) {
    match action {
        Some(Action::Review { id, request }) => (id, request),
        other => panic!("not a request: {other:?}"),
    }
}

fn prepare(id: &str) -> Request {
    Request::Prepare { id: id.to_owned() }
}

/// A dry run, with the local review's `drafts` and the roots in view.
fn dry_run(drafts: Option<(&Path, &[&str])>) -> Request {
    Request::DryRun {
        id: "7".to_owned(),
        drafts: drafts.map(|(file, roots)| {
            let roots = roots.iter().map(|&r| r.to_owned()).collect();
            (file.to_owned(), roots)
        }),
    }
}

fn post(event: &str, summary: Option<&str>) -> Request {
    Request::Post {
        id: "7".to_owned(),
        event: event.to_owned(),
        labels: Vec::new(),
        summary: summary.map(str::to_owned),
    }
}

fn range(action: Option<Action>) -> (String, Option<PathBuf>, Option<(String, String)>) {
    match action {
        Some(Action::Retarget {
            revisions,
            comments,
            moved,
        }) => (revisions.expect("a range").revs.join(" "), comments, moved),
        other => panic!("not a retarget: {other:?}"),
    }
}

/// The command panel, the picker, the notes and the titles say what
/// the review plugin reviews in its own words, and name a review by
/// its short form, its id prefix and its label, else its id; the id
/// is what the plugin's verbs take.
#[test]
fn review_labels_use_the_plugins_words() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    app.reviewing.plugin = Some("test".to_owned());
    described(
        &mut app,
        r#"{"version":1,"command":"mr","noun":"merge request","nouns":"merge requests","short":"MR","id_prefix":"!"}"#,
    );
    let entry = app
        .command_entries()
        .1
        .into_iter()
        .find(|e| e.action == Act::OpenReview)
        .expect("the panel's entry");
    assert_eq!(entry.label, "Open merge request…");
    press(&mut app, Key::Char('p'));
    assert!(
        status(&mut app).contains("not on a merge request; dffy mr <id> opens one"),
        "{}",
        status(&mut app)
    );
    key(&mut app, ' ');
    let (list, _) = plugin_run(press(&mut app, Key::Char('O')));
    assert!(status(&mut app).contains("listing merge requests…"));
    said(
        &mut app,
        list,
        Said::Listed(vec![Listed {
            id: "I8f2c".to_owned(),
            label: Some("12".to_owned()),
            title: "Cache it".to_owned(),
            author: "hubot".to_owned(),
            reason: Reason::Mine,
            group: None,
            drafts: 0,
        }]),
    );
    let picker = screen(&mut app);
    assert!(
        picker.contains(" merge requests ") && picker.contains("!12 ") && !picker.contains('#'),
        "{picker}"
    );
    let (prepare, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, self::prepare("I8f2c"));
    assert!(status(&mut app).contains("fetching MR !12…"));
    let session = Session {
        id: "I8f2c".to_owned(),
        label: Some("12".to_owned()),
        ..session(&dir, HEAD, None)
    };
    said(&mut app, prepare, Said::Session(session));
    assert!(status(&mut app).contains("MR !12 · Spell out five"));
    app.message = None;
    assert!(status(&mut app).contains("MR !12 · fetching threads"));
    assert_eq!(app.description, "MR !12");
    app.reviewing.job = None;
    let (_, request) = plugin_run(press(&mut app, Key::Char('p')));
    assert_eq!(
        request,
        Request::Pull {
            id: "I8f2c".to_owned()
        }
    );
}

/// Without a description, a review reads as one by its id, in words
/// any host's would fit.
#[test]
fn an_undescribed_plugin_reviews_reviews() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    app.reviewing.plugin = Some("test".to_owned());
    press(&mut app, Key::Char('p'));
    assert!(status(&mut app).contains("not on a review"));
    let entry = (app.command_entries().1.into_iter())
        .find(|e| e.action == Act::OpenReview)
        .expect("the panel's entry");
    assert_eq!(entry.label, "Open review…");
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(&mut app, prepare, Said::Progress("1/2".to_owned()));
    app.message = None;
    let shown = status(&mut app);
    assert!(shown.contains("review 7 · fetching threads 1/2"), "{shown}");
}

/// "Open pull request…" lists the PRs, grouped by why they're listed:
/// the reasons dffy knows first, then the plugin's own groups in the
/// order they came, by its titles. `enter` prepares one, whose session
/// line switches the viewer to its range and comments in place. Its
/// threads keep downloading, which the status bar shows. Runs replaced
/// since say nothing.
#[test]
fn the_pr_picker_opens_a_pr_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 30);
    described(&mut app, DESCRIBED);
    screen(&mut app);
    key(&mut app, ' ');
    assert_eq!(press(&mut app, Key::Char('O')), None);
    assert!(status(&mut app).contains("no review plugin set"));
    app.reviewing.plugin = Some("test".to_owned());
    key(&mut app, ' ');
    let (list, request) = plugin_run(press(&mut app, Key::Char('O')));
    assert_eq!(request, Request::List);
    let listed = |id: usize, reason, group: Option<&str>| Listed {
        id: id.to_string(),
        label: None,
        title: format!("Change {id}"),
        author: "hubot".to_owned(),
        reason,
        group: group.map(str::to_owned),
        drafts: usize::from(id == 9) * 2,
    };
    said(
        &mut app,
        list,
        Said::Listed(vec![
            listed(3, Reason::Other, Some("Starred")),
            listed(11, Reason::Mine, None),
            listed(5, Reason::Other, Some("Mentions you")),
            listed(9, Reason::ReviewRequested, None),
            listed(4, Reason::Other, Some("Starred")),
            listed(7, Reason::CurrentBranch, None),
        ]),
    );
    let shown = screen(&mut app);
    let at = |text: &str| {
        shown
            .find(text)
            .unwrap_or_else(|| panic!("{text}:\n{shown}"))
    };
    assert!(at("This branch") < at("#7") && at("#7") < at("Waiting on your review"));
    assert!(at("#9") < at("Yours") && at("Yours") < at("#11"));
    assert!(at("#11") < at("Starred") && at("Starred") < at("#3"));
    assert!(at("#3") < at("#4") && at("#4") < at("Mentions you"));
    assert!(at("Mentions you") < at("#5"));
    assert!(
        at("#9") < at("2 drafts") && at("2 drafts") < at("Yours"),
        "{shown}"
    );
    press(&mut app, Key::Down);
    let (prepare, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, self::prepare("9"));
    assert!(app.reviews.is_none());
    assert!(status(&mut app).contains("fetching PR #9…"));
    assert_eq!(
        said(&mut app, list, Said::Progress("late".to_owned())),
        None
    );
    let nine = Session {
        id: "9".to_owned(),
        ..session(&dir, HEAD, None)
    };
    let (revisions, comments, _) = range(said(&mut app, prepare, Said::Session(nine)));
    assert_eq!(revisions, format!("{BASE}...{HEAD}"));
    assert_eq!(comments, Some(dir.path().join("comments.json")));
    said(&mut app, prepare, Said::Progress("100/312".to_owned()));
    app.message = None;
    assert!(
        status(&mut app).contains("PR #9 · fetching threads 100/312"),
        "{}",
        status(&mut app)
    );
    said(
        &mut app,
        prepare,
        Said::Done(Err("threads incomplete".to_owned())),
    );
    let shown = status(&mut app);
    assert!(shown.contains("PR #9: threads incomplete"), "{shown}");
    app.message = None;
    assert!(!status(&mut app).contains("fetching"));
}

/// What the plugin pushes about the review shown goes in the status
/// bar, new comments and a moved head; a push about another review, or
/// a head that's the session's, says nothing. `p` pulls the moved head
/// and shows the new range in place, through the same file of comments.
/// The review's words name the keys bound to what they point to: a key
/// bound elsewhere shows as that key, and a hint whose action has no
/// key is left out.
#[test]
fn hints_name_the_keys_bound() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    described(&mut app, DESCRIBED);
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(&mut app, prepare, Said::Done(Ok(())));
    let moved = Push::HeadMoved {
        id: "7".to_owned(),
        head: "cccc333".to_owned(),
    };
    app.event(Event::ReviewPush(moved));
    app.message = None;
    for bind in ["p=unbind", "u=review_pull", "m=unbind", "n=next_thread"] {
        app.keys.bind(bind).expect("valid");
    }
    let shown = status(&mut app);
    assert!(
        shown.contains("PR #7 updated · new commits · u to update"),
        "{shown}"
    );
    let hint = |hint| hint_words(&app.keys, hint);
    assert_eq!(
        hint(Hint::StepThreads).as_deref(),
        Some("n/M go between threads")
    );
    assert_eq!(hint(Hint::OtherBranches).as_deref(), Some("b lists them"));
    app.keys.bind("u=unbind").expect("valid");
    app.dirty = true;
    let shown = status(&mut app);
    assert!(
        shown.contains("PR #7 updated · new commits") && !shown.contains("to update"),
        "{shown}"
    );
}

/// Nothing asks the plugin on a timer.
#[test]
fn a_pushed_head_shows_and_p_updates_the_pr_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    described(&mut app, DESCRIBED);
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(&mut app, prepare, Said::Done(Ok(())));
    app.reviewing.mode.as_mut().expect("a PR").plans.due = None;
    let deadline = app.deadline().expect("idle, later");
    assert!(deadline + Duration::from_secs(5) > Instant::now() + session::IDLE_AFTER);
    assert_eq!(app.event(Event::Tick), None, "no request on a timer");

    let push = |app: &mut App, push| app.event(Event::ReviewPush(push));
    let id = || "7".to_owned();
    push(&mut app, Push::CommentsChanged { id: "9".to_owned() });
    push(
        &mut app,
        Push::HeadMoved {
            id: id(),
            head: HEAD.to_owned(),
        },
    );
    app.message = None;
    assert!(!status(&mut app).contains("p to update"));
    push(&mut app, Push::CommentsChanged { id: id() });
    let shown = status(&mut app);
    assert!(
        shown.contains("PR #7 · new comments · p to update"),
        "{shown}"
    );
    push(
        &mut app,
        Push::HeadMoved {
            id: id(),
            head: "cccc333".to_owned(),
        },
    );
    let shown = status(&mut app);
    assert!(
        shown.contains("PR #7 updated · new commits · p to update"),
        "{shown}"
    );
    push(&mut app, Push::Log("checking PR #7 failed".to_owned()));
    assert!(status(&mut app).contains("checking PR #7 failed"));

    let (pull, request) = plugin_run(press(&mut app, Key::Char('p')));
    assert_eq!(request, Request::Pull { id: id() });
    let (revisions, comments, moved) = range(said(
        &mut app,
        pull,
        Said::Session(session(&dir, "cccc333", None)),
    ));
    assert_eq!(revisions, format!("{BASE}...cccc333"));
    assert_eq!(comments, None, "the same comments");
    assert_eq!(moved, Some((HEAD.to_owned(), "cccc333".to_owned())));
    app.review_updated(Some(3));
    let shown = status(&mut app);
    assert!(shown.contains("PR #7 updated · 3 new commits"), "{shown}");
    said(&mut app, pull, Said::Done(Ok(())));
    app.message = None;
    assert!(!status(&mut app).contains("p to update"));

    // Up to date is said once the threads are in, not at the session
    // line, which comes before them.
    let (again, _) = plugin_run(press(&mut app, Key::Char('p')));
    assert_eq!(
        said(
            &mut app,
            again,
            Said::Session(session(&dir, "cccc333", None))
        ),
        None
    );
    assert!(!status(&mut app).contains("up to date"));
    said(&mut app, again, Said::Done(Ok(())));
    assert!(status(&mut app).contains("PR #7 is up to date"));
}

/// Without a key for a while on a review you're idle, which the plugin
/// is told once, waking nothing more; the next key ends it.
#[test]
fn idle_is_told_once_and_a_key_ends_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    described(&mut app, DESCRIBED);
    assert_eq!(app.deadline(), None, "not on a review");
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(&mut app, prepare, Said::Done(Ok(())));
    app.reviewing.mode.as_mut().expect("a PR").plans.due = None;
    app.last_input = (Instant::now().checked_sub(session::IDLE_AFTER))
        .and_then(|at| at.checked_sub(Duration::from_secs(1)))
        .expect("the clock is past it");
    app.message = None;
    assert_eq!(app.take_idle(), None);
    assert_eq!(app.event(Event::Tick), None);
    assert_eq!(app.take_idle(), Some(true));
    assert_eq!(app.deadline(), None, "nothing to wake for");
    app.event(Event::Tick);
    assert_eq!(app.take_idle(), None, "told once");
    key(&mut app, 'j');
    assert_eq!(app.take_idle(), Some(false));
    assert!(app.deadline().is_some());
}

/// `P` shows what goes, from the plan the dialog asks for, what waits
/// on the host, a warning when the PR moved, and the plugin's events,
/// starting on the first that submits; a letter picks one, `s` writes
/// the summary, `enter` posts. The status bar follows the post, and `o`
/// opens the review it made. The summary is kept in the comments file
/// until a submitting event sends it.
#[test]
fn the_post_dialog_sends_the_event_and_summary_picked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    assert_eq!(path, dir.path().join("comments.json"), "the session's");
    app.reviewing.mode.as_mut().expect("a PR").moved = Some("cccc333".to_owned());
    let (plan, request) = plugin_run(press(&mut app, Key::Char('P')));
    assert_eq!(request, dry_run(None));
    let shown = screen(&mut app);
    assert!(shown.contains("post · PR #7"), "{shown}");
    assert!(
        shown.contains("5 comments") && shown.contains("checking what can be posted…"),
        "{shown}"
    );
    said(
        &mut app,
        plan,
        Said::Planned {
            line: 5,
            file: 0,
            skip: 0,
            pending: 2,
            resolves: 1,
        },
    );
    said(&mut app, plan, Said::Done(Ok(())));
    let shown = screen(&mut app);
    assert!(
        shown.contains("5 comments · 1 resolve change · 2 pending on PR #7"),
        "{shown}"
    );
    assert!(!shown.contains("checking"), "{shown}");
    assert!(shown.contains("PR #7 has new commits"), "{shown}");
    assert!(
        shown.contains("(•) c  comment") && shown.contains("( ) r  request changes"),
        "the plugin's events, the first that submits picked: {shown}"
    );
    key(&mut app, 'a');
    key(&mut app, 's');
    typed(&mut app, "Ship it");
    press(&mut app, Key::Esc);
    assert!(app.posting.is_some(), "esc leaves the summary first");
    let (post, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, self::post("approve", Some("Ship it")));
    assert_eq!(read_comments(&path).summary.as_deref(), Some("Ship it"));
    said(&mut app, post, Said::Progress("3/6".to_owned()));
    app.message = None;
    assert!(status(&mut app).contains("PR #7 · posting 3/6"));
    assert_eq!(press(&mut app, Key::Char('p')), None, "no update meanwhile");
    let url = "https://github.com/acme/widgets/pull/7#pullrequestreview-1";
    said(
        &mut app,
        post,
        Said::Posted {
            posted: 5,
            failed: 0,
            pending: 2,
            url: Some(url.to_owned()),
            last_reviewed_head: Some(HEAD.to_owned()),
            labels: None,
        },
    );
    said(&mut app, post, Said::Done(Ok(())));
    let shown = status(&mut app);
    assert!(shown.contains(&format!("review sent · {url}")), "{shown}");
    assert_eq!(
        press(&mut app, Key::Char('o')),
        Some(Action::Browse(url.to_owned()))
    );
    let mode = app.reviewing.mode.as_ref().expect("a PR");
    assert_eq!(mode.session.last_reviewed_head.as_deref(), Some(HEAD));
    assert_eq!(read_comments(&path).summary, None, "sent");
    // The next opening starts on the first event that submits again;
    // a plan that fails says so where the counts' check would.
    app.reviewing.job = None;
    let (plan, ..) = plugin_run(press(&mut app, Key::Char('P')));
    let shown = screen(&mut app);
    assert!(shown.contains("(•) c  comment"), "{shown}");
    said(&mut app, plan, Said::Done(Err("no session".to_owned())));
    let shown = screen(&mut app);
    assert!(
        shown.contains("couldn't check what can be posted: no session"),
        "{shown}"
    );
}

/// A plugin whose events all leave the review open starts the dialog on
/// the one last sent.
#[test]
fn without_a_submitting_event_the_dialog_starts_on_the_last_sent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 30);
    described(
        &mut app,
        r#"{"version":1,"command":"cr","noun":"change","nouns":"changes","events":[{"id":"draft","key":"d","label":"keep as draft","done":"kept","submits":false},{"id":"share","key":"h","label":"share","done":"shared","submits":false}]}"#,
    );
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    press(&mut app, Key::Char('P'));
    assert!(screen(&mut app).contains("(•) d  keep as draft"));
    key(&mut app, 'h');
    let (post, ..) = plugin_run(press(&mut app, Key::Enter));
    said(&mut app, post, Said::Done(Ok(())));
    press(&mut app, Key::Char('P'));
    assert!(screen(&mut app).contains("(•) h  share"));
}

/// A post that changed your votes says the labels as they are now: the
/// next dialog starts on them, and a vote left there sends no label.
#[test]
fn the_votes_a_post_left_start_the_next_dialog() {
    use crate::plugin::review::{LabelValue, ReviewLabel};
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    let verified = |vote| {
        vec![ReviewLabel {
            name: "Verified".to_owned(),
            values: [(0, "No score"), (1, "Works")]
                .map(|(value, label)| LabelValue {
                    value,
                    label: label.to_owned(),
                })
                .to_vec(),
            vote,
        }]
    };
    let session = Session {
        labels: verified(0),
        ..session(&dir, HEAD, None)
    };
    app.start_review("test".to_owned(), session);
    app.reviewing.job = None;
    press(&mut app, Key::Char('P'));
    assert!(screen(&mut app).contains("Verified  ‹0›  No score"));
    let (post, _) = plugin_run(press(&mut app, Key::Enter));
    said(
        &mut app,
        post,
        Said::Posted {
            posted: 5,
            failed: 0,
            pending: 0,
            url: None,
            last_reviewed_head: None,
            labels: Some(verified(1)),
        },
    );
    said(&mut app, post, Said::Done(Ok(())));
    press(&mut app, Key::Char('P'));
    assert!(screen(&mut app).contains("Verified  ‹+1›  Works"));
    let (_, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, self::post("comment", None));
}

/// The summary outlasts the dialog, in the comments file, and comes
/// back with it; an event that leaves the review open keeps it.
#[test]
fn the_summary_is_kept_until_a_review_sends_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    press(&mut app, Key::Char('P'));
    key(&mut app, 's');
    typed(&mut app, "Two nits");
    press(&mut app, Key::Esc);
    press(&mut app, Key::Esc);
    assert!(app.posting.is_none());
    assert_eq!(read_comments(&path).summary.as_deref(), Some("Two nits"));
    press(&mut app, Key::Char('P'));
    assert!(screen(&mut app).contains("Two nits"));
    key(&mut app, 'p');
    let (post, ..) = plugin_run(press(&mut app, Key::Enter));
    said(
        &mut app,
        post,
        Said::Posted {
            posted: 5,
            failed: 0,
            pending: 0,
            url: None,
            last_reviewed_head: None,
            labels: None,
        },
    );
    said(&mut app, post, Said::Done(Ok(())));
    assert_eq!(read_comments(&path).summary.as_deref(), Some("Two nits"));
    let mode = app.reviewing.mode.as_ref().expect("a PR");
    assert_eq!(
        mode.session.last_reviewed_head, None,
        "the plugin records the head it reviewed, and recorded none"
    );
}

/// Comments in your pending review on the host show `pending`, and the
/// status bar says how many wait and that `P` submits them; with no
/// draft, an event that submits still goes.
#[test]
fn pending_comments_show_and_p_submits_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let json = r#"{"version":1,"comments":[
{"id":"p1","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":null,"new":4},"end":{"old":null,"new":4},"text":"Waiting","author":"","created":"2026-11-17T10:00:00Z","ref":"github:1","pending":true,"anchor":{"before":[],"lines":["line four"],"after":[]}}
]}"#;
    let mut app = reviewed(&dir, json, (160, 30));
    described(&mut app, DESCRIBED);
    app.reviewing.job = None;
    app.message = None;
    let shown = screen(&mut app);
    assert!(shown.contains("you · 2h ago · pending"), "{shown}");
    assert!(
        status(&mut app).contains("review 7 · 1 pending · P submits"),
        "{}",
        status(&mut app)
    );
    let (plan, ..) = plugin_run(press(&mut app, Key::Char('P')));
    said(
        &mut app,
        plan,
        Said::Planned {
            line: 0,
            file: 0,
            skip: 0,
            pending: 1,
            resolves: 0,
        },
    );
    said(&mut app, plan, Said::Done(Ok(())));
    assert!(screen(&mut app).contains("1 pending on review 7"));
    key(&mut app, 'a');
    let (_, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, post("approve", None));
}

/// The local review's file: a draft thread on new line 2 with a reply,
/// a draft whose line isn't in the PR's diff, and a posted comment.
const LOCAL: &str = r#"{"version":1,"comments":[
{"id":"l1","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":2,"new":2},"end":{"old":2,"new":2},"text":"Local one","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":["line 1"],"lines":["line 2"],"after":["line 3"]}},
{"id":"l2","path":"src/some/deeply/nested/module/file.rs","text":"Local reply","author":"","created":"2026-11-17T10:05:00Z","in_reply_to":"l1"},
{"id":"l9","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":null,"new":30},"end":{"old":null,"new":30},"text":"Not on this diff","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":[],"lines":["only in the working tree"],"after":[]}},
{"id":"lp","path":"src/some/deeply/nested/module/file.rs","text":"Posted from here","author":"","created":"2026-11-17T10:00:00Z","ref":"github:9"}
]}"#;

/// The PR's file: your draft on new line 4, and a posted comment on
/// new line 50.
const PR_FILE: &str = r#"{"version":1,"comments":[
{"id":"pd","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":null,"new":4},"end":{"old":null,"new":4},"text":"PR draft","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":[],"lines":["line four"],"after":[]}},
{"id":"pp","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":null,"new":50},"end":{"old":null,"new":50},"text":"Fifty?","author":"octocat","created":"2026-11-17T10:00:00Z","ref":"github:1","anchor":{"before":[],"lines":["line fifty"],"after":[]}}
]}"#;

/// A viewer on PR #7 with [`PR_FILE`], plain dffy's [`LOCAL`] beside it,
/// drawn once they're placed. Returns the PR's file and the local one.
fn pr_with_local(dir: &tempfile::TempDir) -> (App, PathBuf, PathBuf) {
    let (pr, local) = (
        dir.path().join("comments.json"),
        dir.path().join("local.json"),
    );
    std::fs::write(&pr, PR_FILE).expect("write");
    std::fs::write(&local, LOCAL).expect("write");
    let mut app = app(100, 30);
    described(&mut app, DESCRIBED);
    app.set_local_comments(Some(local.clone()));
    app.start_review("test".to_owned(), session(dir, HEAD, None));
    app.reviewing.job = None;
    app.set_comments(pr.clone());
    screen(&mut app);
    (app, pr, local)
}

/// A local draft written elsewhere comes into view once it's placed,
/// and the drafts are planned again then.
#[test]
fn a_local_draft_placed_later_is_planned_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _, local) = pr_with_local(&dir);
    let due = |app: &App| app.reviewing.mode.as_ref().and_then(|m| m.plans.due);
    app.reviewing.mode.as_mut().expect("a PR").plans.due = None;
    let draft = r#"{"id":"l7","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":null,"new":2},"end":{"old":null,"new":2},"text":"Written elsewhere","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":[],"lines":["line 3"],"after":[]}}"#;
    let drafts = LOCAL.strip_suffix("\n]}").expect("a list");
    std::fs::write(&local, format!("{drafts},\n{draft}\n]}}")).expect("write");
    app.event(Event::Comments);
    assert_eq!(due(&app), None, "not in view yet");
    assert!(screen(&mut app).contains("Written elsewhere"));
    assert!(due(&app).is_some());
}

/// On a PR, the local review's drafts show with a `branch draft` chip and
/// count as drafts; one whose lines aren't found shows under the file's
/// last row and in the `i` list, saying it stays, and a posted one isn't
/// brought. Edits and replies in a local thread go to the local file, a new
/// comment to the PR's.
#[test]
fn local_drafts_show_on_a_pr_and_are_written_where_they_live() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, pr, local) = pr_with_local(&dir);
    let shown = screen(&mut app);
    assert!(shown.contains("draft · branch draft"), "{shown}");
    assert!(shown.contains("Local reply"), "{shown}");
    assert!(!shown.contains("Posted from here"), "{shown}");
    assert_eq!(
        app.sending(true),
        session::Sending {
            comments: 3,
            local: 2,
            staying: 1,
            ..session::Sending::default()
        }
    );
    assert_eq!(
        posting(&app, "l9"),
        Some((
            Role::Warn,
            "stays on the branch: its lines aren't here".to_owned()
        ))
    );
    key(&mut app, 'G');
    let shown = screen(&mut app);
    assert!(shown.contains("Not on this diff"), "{shown}");
    assert!(shown.contains("stays on the branch"), "{shown}");
    key(&mut app, 'g');
    key(&mut app, 'i');
    let list = screen(&mut app);
    assert!(
        list.contains("4 threads") && list.contains(":30 moved branch dr"),
        "{list}"
    );
    press(&mut app, Key::Esc);

    let pr_before = std::fs::read(&pr).expect("read");
    app.follow_thread("l1");
    // `e` takes the thread's last draft: the reply.
    key(&mut app, 'e');
    typed(&mut app, " more");
    press(&mut app, Key::Esc);
    key(&mut app, 'r');
    typed(&mut app, "Again");
    press(&mut app, Key::Esc);
    let replies: Vec<_> = (read_comments(&local).comments.into_iter())
        .filter(|c| c.in_reply_to.as_deref() == Some("l1"))
        .map(|c| c.text)
        .collect();
    assert_eq!(replies, ["Local reply more", "Again"]);
    assert_eq!(std::fs::read(&pr).expect("read"), pr_before, "not the PR's");
    press(&mut app, Key::Esc);
    key(&mut app, 'c');
    typed(&mut app, "New here");
    press(&mut app, Key::Esc);
    let texts: Vec<String> = read_comments(&pr)
        .comments
        .into_iter()
        .map(|c| c.text)
        .collect();
    assert_eq!(texts, ["PR draft", "Fifty?", "New here"]);
    assert_eq!(read_comments(&local).comments.len(), 5);
}

/// The post dialog plans the local drafts in view with the review's,
/// through `--drafts`, and sends them only with `l`: those written
/// before the session started aren't sent unasked. Sending moves them
/// into the PR's file, with new ids, before `post` runs, and out of the
/// local file after; when the PR's file can't take them, nothing is
/// posted, the dialog stays and both files are as they were.
#[test]
fn posting_moves_the_local_drafts_into_the_prs_file_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, pr, local) = pr_with_local(&dir);
    let (check, request) = plugin_run(press(&mut app, Key::Char('P')));
    assert_eq!(request, dry_run(Some((&local, &["l1"]))), "the one in view");
    said(&mut app, check, draft("l1", Plan::File));
    said(
        &mut app,
        check,
        Said::Planned {
            line: 2,
            file: 1,
            skip: 0,
            pending: 0,
            resolves: 0,
        },
    );
    said(&mut app, check, Said::Done(Ok(())));
    let shown = screen(&mut app);
    assert!(
        shown.contains("1 comment") && shown.contains("l  [ ] with the 2 branch drafts"),
        "written before the session: {shown}"
    );
    key(&mut app, 'l');
    let shown = screen(&mut app);
    assert!(
        shown.contains("3 comments · 1 as a file comment")
            && shown.contains("l  [x] with the 2 branch drafts")
            && shown.contains("! 1 branch draft stays, lines not found · v goes to them"),
        "{shown}"
    );

    std::fs::write(&pr, r#"{"version":2,"comments":[]}"#).expect("write");
    let local_before = std::fs::read(&local).expect("read");
    assert_eq!(press(&mut app, Key::Enter), None);
    assert!(app.posting.is_some(), "the dialog stays");
    assert!(status(&mut app).contains("nothing posted"));
    assert_eq!(std::fs::read(&local).expect("read"), local_before);
    assert_eq!(
        std::fs::read_to_string(&pr).expect("read"),
        r#"{"version":2,"comments":[]}"#
    );

    std::fs::write(&pr, PR_FILE).expect("write");
    let (_, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, post("comment", None));
    let left: Vec<String> = read_comments(&local)
        .comments
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(left, ["l9", "lp"]);
    let moved = read_comments(&pr).comments;
    let texts: Vec<&str> = moved.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(texts, ["PR draft", "Fifty?", "Local one", "Local reply"]);
    assert!(moved[2].id != "l1" && moved[2].reference.is_none());
    assert_eq!(moved[3].in_reply_to.as_ref(), Some(&moved[2].id));
    let shown = screen(&mut app);
    assert!(
        shown.contains("Local one") && !shown.contains("· local"),
        "{shown}"
    );
}

fn draft(id: &str, plan: Plan) -> Said {
    Said::Draft(Draft {
        id: id.to_owned(),
        path: FILE.to_owned(),
        plan,
    })
}

fn skip(why: &str) -> Plan {
    Plan::Skip(why.to_owned())
}

/// What the heading of [`FILE`]'s comment `id` says posting it does.
fn posting(app: &App, id: &str) -> Option<(Role, String)> {
    let threads = app.comments.as_ref()?.threads(FILE.as_bytes())?;
    let note = threads
        .iter()
        .flat_map(|t| &t.comments)
        .find(|n| n.id == id)?;
    let posting = note.posting.clone()?;
    let role = if posting.warn { Role::Warn } else { Role::Dim };
    Some((role, posting.words))
}

/// The dialog asks for a plan beside the run going on and counts by
/// it, with one warning for the drafts that won't go: `v` closes the
/// dialog on the first, its thread focused, the summary kept; `m` goes
/// on through the threads from there. Each draft's heading says what
/// posting it does.
#[test]
fn the_post_dialog_warns_of_drafts_that_wont_go_and_v_goes_there() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    screen(&mut app);
    let (check, request) = plugin_run(press(&mut app, Key::Char('P')));
    assert_eq!(request, dry_run(None));
    assert!(
        app.reviewing.job.as_ref().is_some_and(|r| r.id == prepare),
        "the prepare goes on"
    );
    said(&mut app, check, draft("d1", Plan::Line));
    said(
        &mut app,
        check,
        draft("d2", skip("its thread isn't posted")),
    );
    said(&mut app, check, draft("f1", skip("")));
    said(&mut app, check, draft("m1", Plan::File));
    said(&mut app, check, Said::Done(Ok(())));
    let shown = screen(&mut app);
    assert!(
        shown.contains("5 comments · 1 as a file comment"),
        "{shown}"
    );
    assert!(
        shown.contains("! 2 drafts left out · v goes to them"),
        "{shown}"
    );
    key(&mut app, 's');
    typed(&mut app, "Later");
    press(&mut app, Key::Esc);

    key(&mut app, 'v');
    assert!(app.posting.is_none());
    assert_eq!(read_comments(&path).summary.as_deref(), Some("Later"));
    assert_eq!(focused(&app), Some("d1"), "the reply's thread");
    let shown = status(&mut app);
    assert!(
        shown.contains("not posted: its thread isn't posted · 1 more · m/M go between threads"),
        "{shown}"
    );
    let shown = screen(&mut app);
    assert!(
        shown.contains("Because.") && shown.contains("draft · not in the review yet"),
        "{shown}"
    );
    assert_eq!(posting(&app, "d1"), None, "posts on its line");
    assert_eq!(
        posting(&app, "d2"),
        Some((Role::Warn, "its thread isn't posted".to_owned())),
        "the plugin's reason"
    );
    assert_eq!(
        posting(&app, "f1"),
        Some((Role::Warn, "not in the review yet".to_owned())),
        "no reason given"
    );
}

/// A draft the host refused before says why in its heading, and the
/// dialog warns of it; a post that leaves drafts says how many were
/// refused, and the review is sent without them.
#[test]
fn refused_drafts_say_why_and_the_post_says_how_it_went() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let refuse = |path: &PathBuf, why: &str| {
        review::apply(
            path,
            vec![Op::Set {
                target: Target::Id("d1".to_owned()),
                change: Box::new(Change {
                    post_error: Some(Some(why.to_owned())),
                    ..Change::default()
                }),
            }],
        )
        .expect("applies");
    };
    refuse(&path, "Line could not be resolved");
    app.event(Event::Comments);
    let shown = screen(&mut app);
    assert!(shown.contains("draft · refused: Line could"), "{shown}");
    let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
    said(&mut app, check, Said::Done(Ok(())));
    assert!(screen(&mut app).contains("! 1 refused before · v goes to them"));
    key(&mut app, 'a');
    let (post, ..) = plugin_run(press(&mut app, Key::Enter));
    said(
        &mut app,
        post,
        draft("d1", skip("Line could not be resolved")),
    );
    said(&mut app, post, draft("g1", skip("file not in the PR")));
    said(
        &mut app,
        post,
        Said::Posted {
            posted: 3,
            failed: 2,
            pending: 0,
            url: None,
            last_reviewed_head: None,
            labels: None,
        },
    );
    refuse(&path, "Line could not be resolved");
    said(&mut app, post, Said::Done(Ok(())));
    let shown = status(&mut app);
    assert!(
        shown.contains("review sent · 1 comment refused, kept as drafts · 1 draft left out"),
        "{shown}"
    );
}

/// An event that leaves the review open on the host says it in its own
/// words; with nothing posted, it says so.
#[test]
fn an_event_that_leaves_the_review_open_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    for (posted, failed, said_after) in [
        (2, 1, "review pending · 1 draft left out"),
        (0, 0, "nothing posted"),
    ] {
        if let Some((check, ..)) = press(&mut app, Key::Char('P')).map(|a| plugin_run(Some(a))) {
            said(&mut app, check, Said::Done(Ok(())));
        }
        key(&mut app, 'p');
        let (post, request) = plugin_run(press(&mut app, Key::Enter));
        assert_eq!(request, self::post("pending", None));
        if failed > 0 {
            said(&mut app, post, draft("g1", skip("file not in the PR")));
        }
        said(
            &mut app,
            post,
            Said::Posted {
                posted,
                failed,
                pending: 0,
                url: None,
                last_reviewed_head: None,
                labels: None,
            },
        );
        said(&mut app, post, Said::Done(Ok(())));
        let shown = status(&mut app);
        assert!(shown.contains(said_after), "{shown}");
    }
}

/// A plugin that names no events posts with one, `post`.
#[test]
fn without_events_a_review_is_posted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 30);
    described(
        &mut app,
        r#"{"version":1,"command":"cr","noun":"change","nouns":"changes"}"#,
    );
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
    said(&mut app, check, Said::Done(Ok(())));
    let shown = screen(&mut app);
    assert!(shown.contains("(•) p  post"), "{shown}");
    let (post, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, self::post("post", None));
    said(
        &mut app,
        post,
        Said::Posted {
            posted: 5,
            failed: 0,
            pending: 0,
            url: None,
            last_reviewed_head: None,
            labels: None,
        },
    );
    said(&mut app, post, Said::Done(Ok(())));
    assert!(status(&mut app).contains("change 7 · review sent"));
}

/// While a post runs, the first quit says how far it got and that
/// another stops it; the second quits. After the post, one quit does.
#[test]
fn a_quit_while_posting_asks_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
    said(&mut app, check, Said::Done(Ok(())));
    let (post, ..) = plugin_run(press(&mut app, Key::Enter));
    said(&mut app, post, Said::Progress("37/200".to_owned()));
    assert_eq!(press(&mut app, Key::Char('q')), None);
    assert!(status(&mut app).contains("still posting 37/200 · q again stops it"));
    assert_eq!(press(&mut app, Key::Char('q')), Some(Action::Quit));
    said(&mut app, post, Said::Done(Ok(())));
    assert_eq!(press(&mut app, Key::Char('q')), Some(Action::Quit));
}

/// The plugin takes requests side by side, so a post waits for the
/// update running: the dialog stays, its summary with it, and posts
/// once the update is done.
#[test]
fn a_post_waits_for_the_update_running() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let (pull, request) = plugin_run(press(&mut app, Key::Char('p')));
    assert_eq!(request, Request::Pull { id: "7".to_owned() });
    if let Some((check, ..)) = press(&mut app, Key::Char('P')).map(|a| plugin_run(Some(a))) {
        said(&mut app, check, Said::Done(Ok(())));
    }
    key(&mut app, 's');
    typed(&mut app, "Nits");
    press(&mut app, Key::Esc);
    key(&mut app, 'c');
    assert_eq!(press(&mut app, Key::Enter), None, "nothing asked");
    let shown = status(&mut app);
    assert!(
        shown.contains("still updating; post once it's done"),
        "{shown}"
    );
    assert!(app.posting.is_some(), "the dialog stays");
    assert_eq!(read_comments(&path).summary, None, "nothing written");
    said(&mut app, pull, Said::Done(Ok(())));
    let (_, request) = plugin_run(press(&mut app, Key::Enter));
    assert_eq!(request, post("comment", Some("Nits")));
}

/// While a post runs, the plugin writes what it posted to the file: a
/// draft edited then would go up as it was, or not be found. The text
/// stays in its box, saved once the post is done, and an update after
/// keeps it.
#[test]
fn a_draft_edited_while_posting_waits_in_its_box() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
    said(&mut app, check, Said::Done(Ok(())));
    let (post, ..) = plugin_run(press(&mut app, Key::Enter));
    app.follow_thread("d1");
    key(&mut app, 'e');
    typed(&mut app, " Mostly.");
    press(&mut app, Key::Esc);
    assert!(app.doc.writing().is_some(), "still open");
    assert!(status(&mut app).contains("posting; edit once it's done"));
    press(&mut app, Key::Esc);
    assert!(
        app.doc.writing().is_some(),
        "a second esc doesn't discard it"
    );
    let text = |path: &Path| {
        (read_comments(path).comments.into_iter())
            .find(|c| c.id == "d2")
            .map(|c| c.text)
    };
    assert_eq!(text(&path).as_deref(), Some("Because."));
    said(&mut app, post, Said::Done(Ok(())));
    press(&mut app, Key::Esc);
    assert!(app.doc.writing().is_none());
    assert_eq!(text(&path).as_deref(), Some("Because. Mostly."));
    let (pull, ..) = plugin_run(press(&mut app, Key::Char('p')));
    said(&mut app, pull, Said::Done(Ok(())));
    assert_eq!(text(&path).as_deref(), Some("Because. Mostly."));
}

/// A signal or the terminal going away keeps the post dialog's summary
/// for the next time, as closing the dialog does.
#[test]
fn stopping_keeps_the_post_dialogs_summary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    press(&mut app, Key::Char('P'));
    key(&mut app, 's');
    typed(&mut app, "Half written");
    app.interrupted();
    assert_eq!(
        read_comments(&path).summary.as_deref(),
        Some("Half written")
    );
}

/// A thread of yours deleted on the host, kept for your draft reply in
/// it, isn't posted again as a new thread unasked: its heading says the
/// host deleted it, and the post dialog warns of it.
#[test]
fn a_thread_of_yours_deleted_on_the_host_is_flagged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let text = |s: &str| plugin::json::Value::String(s.to_owned());
    let mine = Op::Upsert {
        fields: vec![
            ("ref".to_owned(), text("github:1")),
            ("author".to_owned(), text("")),
        ],
        reply_to: None,
    };
    review::apply(&path, vec![mine]).expect("yours");
    let reply = Op::Add {
        comment: Box::new(review::Comment {
            text: "Still?".to_owned(),
            ..review::Comment::default()
        }),
        reply_to: Some(Target::Id("p1".to_owned())),
    };
    review::apply(&path, vec![reply]).expect("replies");
    app.event(Event::Comments);
    let before = app.sending(true);
    review::apply(&path, vec![Op::Gone(vec!["github:1".to_owned()])]).expect("gone");
    app.event(Event::Comments);
    let sending = app.sending(true);
    assert_eq!(
        (sending.comments, sending.refused),
        (before.comments + 1, before.refused + 1)
    );
    let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
    said(&mut app, check, Said::Done(Ok(())));
    let shown = screen(&mut app);
    assert!(shown.contains("refused before · v goes to them"), "{shown}");
    assert_eq!(
        posting(&app, "p1"),
        Some((Role::Warn, "refused: deleted on the host".to_owned()))
    );
}

/// The drafts are planned in the background a moment after they
/// change, never per frame, and the plan's words go in their
/// headings; a plan that fails says nothing.
#[test]
fn drafts_are_planned_in_the_background_after_they_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let due = app.deadline().expect("a plan is due");
    assert!(due <= Instant::now() + Duration::from_secs(1), "soon");
    app.reviewing.mode.as_mut().expect("a PR").plans.due = Some(Instant::now());
    let (plan, request) = plugin_run(app.event(Event::Tick));
    assert_eq!(request, dry_run(None));
    assert_eq!(app.event(Event::Tick), None, "one at a time");
    said(&mut app, plan, draft("d1", Plan::File));
    said(&mut app, plan, Said::Done(Ok(())));
    let file_comment = Some((Role::Dim, "posts as file comment".to_owned()));
    assert_eq!(posting(&app, "d1"), file_comment);
    assert_eq!(app.event(Event::Tick), None, "nothing changed since");
    app.follow_thread("d1");

    key(&mut app, 'r');
    typed(&mut app, "More");
    press(&mut app, Key::Esc);
    app.reviewing.mode.as_mut().expect("a PR").plans.due = Some(Instant::now());
    let (plan, ..) = plugin_run(app.event(Event::Tick));
    app.message = None;
    said(&mut app, plan, Said::Done(Err("nope".to_owned())));
    assert!(!status(&mut app).contains("nope"), "{}", status(&mut app));
    assert_eq!(posting(&app, "d1"), file_comment, "the last plan stays");
}

/// The plugin's notice, written as its post ends, is what the status
/// bar says after it, over dffy's own words for how it went, which the
/// review's part keeps; whether the watcher read it first or the
/// run's end did.
#[test]
fn the_plugins_notice_outlasts_the_post() {
    let url = "https://example.com/review/7";
    for watched in [false, true] {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        app.resize(160, 30);
        described(&mut app, DESCRIBED);
        app.start_review("test".to_owned(), session(&dir, HEAD, None));
        app.reviewing.job = None;
        let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
        said(&mut app, check, Said::Done(Ok(())));
        key(&mut app, 'c');
        let (post, ..) = plugin_run(press(&mut app, Key::Enter));
        said(
            &mut app,
            post,
            Said::Posted {
                posted: 3,
                failed: 0,
                pending: 0,
                url: Some(url.to_owned()),
                last_reviewed_head: None,
                labels: None,
            },
        );
        review::apply(
            &path,
            vec![Op::Notice(Some("a resolve was refused".to_owned()))],
        )
        .expect("applies");
        if watched {
            app.event(Event::Comments);
        }
        said(&mut app, post, Said::Done(Ok(())));
        let shown = status(&mut app);
        assert!(
            shown.contains("a resolve was refused"),
            "{watched}: {shown}"
        );
        assert!(shown.contains("PR #7 · review sent"), "{watched}: {shown}");
    }
}

/// "Since last review" switches to the range from the head last
/// reviewed and back; without one it says so.
#[test]
fn since_last_review_toggles_the_range() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    key(&mut app, ' ');
    assert_eq!(press(&mut app, Key::Char('R')), None);
    assert!(status(&mut app).contains("no review of yours posted"));
    app.reviewing
        .mode
        .as_mut()
        .expect("a PR")
        .session
        .last_reviewed_head = Some("9999aaa".to_owned());
    key(&mut app, ' ');
    let (revisions, ..) = range(press(&mut app, Key::Char('R')));
    assert_eq!(revisions, format!("9999aaa..{HEAD}"));
    assert!(status(&mut app).contains("since your last review: 9999aaa..bbbb222"));
    assert_eq!(app.description, "PR #7 since your review");
    key(&mut app, ' ');
    let (revisions, ..) = range(press(&mut app, Key::Char('R')));
    assert_eq!(revisions, format!("{BASE}...{HEAD}"));
}

/// The branch's PR, found by `detect`, is prepared and brings its
/// comments to plain dffy's range, which stays; "Show whole pull
/// request" shows its base against the working tree, and goes back.
/// With no PR, nothing changes.
#[test]
fn a_detected_pr_keeps_the_range_and_adds_its_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    described(&mut app, DESCRIBED);
    let plain = Revisions {
        unstaged: true,
        ..Revisions::default()
    };
    app.set_selected(plain.clone(), DefaultDiff::Unstaged);
    let title = app.description.clone();
    let (id, request) = plugin_run(Some(app.detect_review("test".to_owned())));
    assert_eq!(request, Request::Detect);
    let (prepare, request) = plugin_run(said(
        &mut app,
        id,
        Said::Detected(Some(session(&dir, HEAD, None))),
    ));
    assert_eq!(request, self::prepare("7"));
    said(&mut app, id, Said::Done(Ok(())));
    match said(&mut app, prepare, Said::Session(session(&dir, HEAD, None))) {
        Some(Action::Retarget {
            revisions,
            comments,
            ..
        }) => {
            assert_eq!(revisions, None, "the range stays");
            assert_eq!(comments, Some(dir.path().join("comments.json")));
        }
        other => panic!("not a retarget: {other:?}"),
    }
    assert_eq!(app.description, title);
    let whole = |app: &mut App| {
        key(app, ' ');
        match press(app, Key::Char('Q')) {
            Some(Action::Retarget {
                revisions: Some(revisions),
                comments: None,
                ..
            }) => revisions,
            other => panic!("not a retarget: {other:?}"),
        }
    };
    let revisions = whole(&mut app);
    assert_eq!(revisions.revs, [BASE]);
    assert!(revisions.merge_base && !revisions.staged);
    assert_eq!(app.description, "PR #7 + working tree");
    assert_eq!(app.reviewing.whole_title(), "Show working tree changes");
    assert_eq!(whole(&mut app), plain);
    assert_eq!(app.description, title);
    assert_eq!(app.reviewing.whole_title(), "Show whole pull request");

    let mut app = self::app(160, 20);
    let (id, _) = plugin_run(Some(app.detect_review("test".to_owned())));
    assert_eq!(said(&mut app, id, Said::Detected(None)), None);
    said(&mut app, id, Said::Done(Ok(())));
    assert!(app.reviewing.mode.is_none() && app.reviewing.job.is_none());
}

/// Listing PRs runs beside a `prepare`, which keeps saying how far it
/// got.
#[test]
fn listing_prs_leaves_the_run_going_on() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app(160, 20);
    described(&mut app, DESCRIBED);
    let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    key(&mut app, ' ');
    let (list, request) = plugin_run(press(&mut app, Key::Char('O')));
    assert_eq!(request, Request::List);
    said(&mut app, prepare, Said::Progress("2/9".to_owned()));
    said(&mut app, list, Said::Done(Ok(())));
    app.message = None;
    let shown = status(&mut app);
    assert!(shown.contains("PR #7 · fetching threads 2/9"), "{shown}");
}

#[test]
fn review_keys_refuse_a_piped_diff_and_need_a_pr() {
    let mut app = app(100, 20);
    assert_eq!(press(&mut app, Key::Char('p')), None);
    assert!(screen(&mut app).contains("not on a review"));
    app.set_piped();
    app.reviewing.plugin = Some("test".to_owned());
    key(&mut app, ' ');
    assert_eq!(press(&mut app, Key::Char('O')), None);
    assert!(screen(&mut app).contains("a piped diff can't be reviewed"));
}

/// On the review plain dffy found, a new comment is the working tree's:
/// it goes to the local review's file, on the branch, until posting
/// moves it.
#[test]
fn on_a_detected_review_new_comments_go_to_the_local_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (pr, local) = (
        dir.path().join("comments.json"),
        dir.path().join("local.json"),
    );
    std::fs::write(&pr, PR_FILE).expect("write");
    let mut app = app(100, 30);
    described(&mut app, DESCRIBED);
    app.set_branch(Some("feature".to_owned()));
    app.set_local_comments(Some(local.clone()));
    let (id, _) = plugin_run(Some(app.detect_review("test".to_owned())));
    let (prepare, ..) = plugin_run(said(
        &mut app,
        id,
        Said::Detected(Some(session(&dir, HEAD, None))),
    ));
    said(&mut app, prepare, Said::Session(session(&dir, HEAD, None)));
    app.set_comments(pr.clone());
    screen(&mut app);
    key(&mut app, 'c');
    typed(&mut app, "Fix this");
    press(&mut app, Key::Esc);
    let written = read_comments(&local).comments;
    assert_eq!(written.len(), 1, "{written:?}");
    assert_eq!(written[0].branch.as_deref(), Some("feature"));
    assert!(
        !read_comments(&pr)
            .comments
            .iter()
            .any(|c| c.text == "Fix this")
    );
    assert!(screen(&mut app).contains("draft · branch draft"));

    // Your open threads go to your agent from the file new ones go to,
    // the review's draft counted as left.
    key(&mut app, ' ');
    key(&mut app, '^');
    let handoff = app.take_handoff().expect("handed");
    assert_eq!(
        (handoff.what, handoff.file),
        (Handed::Open { count: 1, left: 1 }, local)
    );
    app.event(Event::HandedOff {
        started: Ok("sent 1 thread to claude".to_owned()),
        left: Some(1),
        unmarked: None,
    });
    app.resize(200, 30);
    assert!(
        status(&mut app).contains("1 more of yours in the other comments file not sent"),
        "{}",
        status(&mut app)
    );
}

/// A review the host closed or merged says so in the status bar, and
/// the dialog warns before posting on it.
#[test]
fn a_merged_review_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 30);
    described(&mut app, DESCRIBED);
    let merged = Session {
        state: Some(plugin::review::State::Merged),
        ..session(&dir, HEAD, None)
    };
    app.start_review("test".to_owned(), merged);
    app.reviewing.job = None;
    app.message = None;
    assert!(
        status(&mut app).contains("PR #7 · merged"),
        "{}",
        status(&mut app)
    );
    press(&mut app, Key::Char('P'));
    assert!(screen(&mut app).contains("! PR #7 is merged"));
}

/// A `pull` that brings threads says how many, and `m` goes to them
/// first; the session pointer names the review session.
#[test]
fn an_update_names_the_new_threads_and_m_goes_to_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, path) = commented(&dir);
    described(&mut app, DESCRIBED);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    assert_eq!(
        app.session_pointer(),
        Some((path.clone(), "7".to_owned(), HEAD.to_owned()))
    );
    let (pull, ..) = plugin_run(press(&mut app, Key::Char('p')));
    said(&mut app, pull, Said::Session(session(&dir, HEAD, None)));
    review::apply(
        &path,
        vec![Op::Upsert {
            fields: [
                ("ref", "github:9"),
                ("path", FILE),
                ("text", "Brand new"),
                ("author", "octocat"),
            ]
            .map(|(k, v)| (k.to_owned(), plugin::json::Value::String(v.to_owned())))
            .into(),
            reply_to: None,
        }],
    )
    .expect("applies");
    said(&mut app, pull, Said::Done(Ok(())));
    assert!(
        status(&mut app).contains("1 new comment · m goes to them"),
        "{}",
        status(&mut app)
    );
    key(&mut app, 'm');
    let new = read_comments(&path)
        .comments
        .into_iter()
        .find(|c| c.text == "Brand new")
        .expect("imported");
    assert_eq!(app.doc.focused().map(|t| t.root.clone()), Some(new.id));
}

/// A plugin's words are escaped where they're drawn: the status bar shows
/// a control character as `^[`, and the terminal's report carries none.
#[test]
fn a_plugins_words_are_escaped_where_they_are_drawn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.resize(160, 20);
    described(&mut app, DESCRIBED);
    let id = app.start_review("test".to_owned(), session(&dir, HEAD, None));
    said(
        &mut app,
        id,
        Said::Done(Err("no \u{1b}[2J access".to_owned())),
    );
    let shown = screen(&mut app);
    assert!(shown.contains("no ^[[2J access"), "{shown}");
    let report = app.status_report().expect("a report");
    assert!(!report.windows(3).any(|w| w == b"\x1b[2"), "{report:?}");
}

/// While HEAD isn't the review's head, a thread's prompt says the files
/// on disk aren't the review's, for an agent that would read them.
#[test]
fn a_prompt_off_the_reviews_head_says_the_files_differ() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut app, _) = commented(&dir);
    app.start_review("test".to_owned(), session(&dir, HEAD, None));
    app.reviewing.job = None;
    let copy = |app: &mut App| {
        app.follow_thread("d1");
        match press(app, Key::Char('y')) {
            Some(Action::Copy { text, .. }) => String::from_utf8(text).expect("utf-8"),
            other => panic!("{other:?}"),
        }
    };
    let note = "The files on disk aren't this review's version";
    app.set_head(Some(HEAD.to_owned()));
    assert!(!copy(&mut app).contains(note));
    app.event(Event::Head(Some(BASE.to_owned())));
    assert!(copy(&mut app).contains(note));
}

//! The session as a state machine: each test feeds it what a plugin would
//! say and checks the effects it asks of the frontend.

use std::path::Path;

use super::*;
use crate::plugin::review::{Draft, LabelValue};

const BASE: &str = "aaaa111";
const HEAD: &str = "bbbb222";

fn session(file: &Path, head: &str) -> Session {
    Session {
        id: "7".to_owned(),
        label: None,
        title: Some("Spell out five".to_owned()),
        url: None,
        base: BASE.to_owned(),
        head: head.to_owned(),
        last_reviewed_head: None,
        state: None,
        labels: Vec::new(),
        comments: file.to_owned(),
    }
}

fn shown() -> Shown<'static> {
    Shown {
        revisions: None,
        description: "working tree",
    }
}

/// `parts` as one line, each hint by its name: `<Update>`.
fn words(parts: &[Part]) -> String {
    model::say(parts, |hint| Some(format!("<{hint:?}>")))
}

/// A review session's comments file with one draft of yours on
/// `src/lib.rs`, read.
fn drafted(dir: &tempfile::TempDir) -> Comments {
    let path = dir.path().join("7.json");
    std::fs::write(
        &path,
        r#"{"version":1,"comments":[
{"id":"d1","path":"src/lib.rs","text":"Tidy this","author":"","created":"2026-11-17T10:00:00Z"}
]}"#,
    )
    .expect("write");
    let mut comments = Comments::new(path, None, true, None);
    comments.reload().expect("read");
    comments
}

/// The request an effect list asks for, with its id.
fn asked(effects: &[Effect]) -> (u64, &Request) {
    effects
        .iter()
        .find_map(|e| match e {
            Effect::Ask { id, request } => Some((*id, request)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no request in {effects:?}"))
}

fn plugged() -> Reviewing {
    Reviewing::new(Some("test".to_owned()))
}

/// Opening a review picked from the list prepares it; its session line
/// shows the review's range with its comments in place, and its end, with
/// nothing new, says nothing more. Words for a run no longer going on are
/// dropped.
#[test]
fn a_prepared_review_shows_its_range_and_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("7.json");
    let mut r = plugged();
    let now = Instant::now();
    let opened = r.open("7".to_owned(), r.label_of("7"));
    assert_eq!(opened[0], Effect::Say("fetching review 7…".to_owned()));
    let (prepare, request) = asked(&opened);
    assert_eq!(*request, Request::Prepare { id: "7".to_owned() });
    assert_eq!(
        r.said(
            (prepare + 1, Said::Progress("1/2".to_owned())),
            None,
            &shown(),
            now
        ),
        [],
        "another run's"
    );
    let effects = r.said(
        (prepare, Said::Session(session(&file, HEAD))),
        None,
        &shown(),
        now,
    );
    assert_eq!(
        effects,
        [
            Effect::Redraw,
            Effect::Say("review 7 · Spell out five".to_owned()),
            Effect::Show {
                range: Some(Range {
                    revisions: Revisions {
                        revs: vec![format!("{BASE}...{HEAD}")],
                        ..Revisions::default()
                    },
                    description: "review 7".to_owned(),
                    title: "Spell out five".to_owned(),
                }),
                comments: Some(file),
                moved: None,
            }
        ]
    );
    assert_eq!(
        r.status(None).as_deref().map(words).as_deref(),
        Some("review 7 · fetching threads")
    );
    let done = r.said((prepare, Said::Done(Ok(()))), None, &shown(), now);
    assert_eq!(done, [Effect::Redraw]);
    assert!(r.job.is_none());
    assert_eq!(
        r.open("7".to_owned(), r.label_of("7")),
        [Effect::Say("review 7 is open".to_owned())]
    );
}

/// The review `detect` finds for the branch keeps plain `dffy`'s range:
/// only its comments come, and "Show whole review" switches to the base
/// against the working tree.
#[test]
fn a_detected_review_keeps_plain_dffys_range() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("7.json");
    let mut r = Reviewing::default();
    let (detect, request) = r.detect("test".to_owned());
    assert_eq!(request, Request::Detect);
    let now = Instant::now();
    let found = r.said(
        (detect, Said::Detected(Some(session(&file, HEAD)))),
        None,
        &shown(),
        now,
    );
    let (prepare, _) = asked(&found);
    let plain = Revisions {
        revs: vec!["HEAD".to_owned()],
        ..Revisions::default()
    };
    let here = Shown {
        revisions: Some(&plain),
        description: "working tree",
    };
    let effects = r.said(
        (prepare, Said::Session(session(&file, HEAD))),
        None,
        &here,
        now,
    );
    assert_eq!(
        effects[2],
        Effect::Show {
            range: None,
            comments: Some(file),
            moved: None,
        }
    );
    let whole = r.toggle_whole();
    let Some(Effect::Show {
        range: Some(range), ..
    }) = whole.last()
    else {
        panic!("{whole:?}");
    };
    assert_eq!(
        (range.revisions.revs.as_slice(), range.revisions.merge_base),
        ([BASE.to_owned()].as_slice(), true)
    );
    assert_eq!(range.description, "review 7 + working tree");
}

/// An update to the head already shown changes nothing on screen, and its
/// end says the review is up to date; one to a new head shows it, with
/// the heads to count commits between.
#[test]
fn an_update_shows_a_new_head_or_says_nothing_changed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("7.json");
    let mut r = plugged();
    let now = Instant::now();
    let (prepare, _) = r.start(("test".to_owned(), session(&file, HEAD)), None, now);
    r.said((prepare, Said::Done(Ok(()))), None, &shown(), now);

    let (pull, _) = asked(&r.update());
    assert_eq!(
        r.update(),
        [Effect::Say("already updating review 7".to_owned())]
    );
    let same = r.said(
        (pull, Said::Session(session(&file, HEAD))),
        None,
        &shown(),
        now,
    );
    assert_eq!(same, [Effect::Redraw]);
    let done = r.said((pull, Said::Done(Ok(()))), None, &shown(), now);
    assert_eq!(
        done.last(),
        Some(&Effect::Say("review 7 is up to date".to_owned()))
    );

    let (pull, _) = asked(&r.update());
    let moved = r.said(
        (pull, Said::Session(session(&file, "cccc333"))),
        None,
        &shown(),
        now,
    );
    let Some(Effect::Show { moved, .. }) = moved.last() else {
        panic!("{moved:?}");
    };
    assert_eq!(*moved, Some((HEAD.to_owned(), "cccc333".to_owned())));
}

/// The drafts' plan is asked for a moment after they change, never while
/// a post runs; its answer heads each draft and counts what the dialog
/// sends, and a post can't be asked for while one runs.
#[test]
fn the_drafts_plan_runs_when_due_and_heads_the_drafts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut comments = drafted(&dir);
    let file = dir.path().join("7.json");
    let mut r = plugged();
    let now = Instant::now();
    let (prepare, _) = r.start(
        ("test".to_owned(), session(&file, HEAD)),
        Some(&mut comments),
        now,
    );
    assert!(
        r.plan_deadline(Some(&comments)).is_some(),
        "beside the prepare"
    );
    r.said(
        (prepare, Said::Done(Ok(()))),
        Some(&mut comments),
        &shown(),
        now,
    );
    let due = r.plan_deadline(Some(&comments)).expect("a plan due");
    assert_eq!(r.due_plan(Some(&comments), now), [], "not yet");
    let ask = r.due_plan(Some(&comments), due);
    let (plan, request) = asked(&ask);
    assert_eq!(
        *request,
        Request::DryRun {
            id: "7".to_owned(),
            drafts: None
        }
    );
    assert_eq!(r.checked(), Checked::Checking);
    let skip = Said::Draft(Draft {
        id: "d1".to_owned(),
        path: "src/lib.rs".to_owned(),
        plan: Plan::Skip(String::new()),
    });
    r.said((plan, skip), Some(&mut comments), &shown(), now);
    let done = r.said(
        (plan, Said::Done(Ok(()))),
        Some(&mut comments),
        &shown(),
        now,
    );
    assert_eq!(done[0], Effect::Changed(vec!["src/lib.rs".into()]));
    assert_eq!(r.checked(), Checked::Yes);
    let sending = r.sending(Some(&comments), true);
    assert_eq!((sending.comments, sending.left_out), (1, 1));
    assert_eq!(
        sending.warning().as_deref().map(words).as_deref(),
        Some("1 draft left out · <GoToUnposted>")
    );
    assert_eq!(
        r.wont_go(Some(&comments), true),
        [("d1".to_owned(), "not in the review yet".to_owned())]
    );
    assert_eq!(
        Reviewing::go_to_unposted(Some(&comments), &r.wont_go(Some(&comments), true)),
        Effect::Focus {
            root: "d1".to_owned(),
            note: vec![Part::Text("not posted: not in the review yet".to_owned())]
        }
    );

    let (form, _) = r.open_post(Some(&comments));
    let form = form.expect("the dialog");
    let post = Post {
        event: form.events[form.event].clone(),
        labels: &form.labels,
        summary: "",
        local: false,
    };
    let sent = r.send(Some(&mut comments), post).expect("posted");
    asked(&sent);
    assert!(r.posting_now());
    assert_eq!(
        r.update(),
        [
            Effect::Say("updating review 7…".to_owned()),
            Effect::Say("the review is still being posted; wait for it to finish".to_owned())
        ]
    );
    assert!(
        r.plan_deadline(Some(&comments)).is_none(),
        "not while posting"
    );
}

/// A post sends a label only when its vote changed, and takes the labels
/// the plugin says the post left, with the head it recorded; its end
/// says how it went and keeps saying it until the next.
#[test]
fn a_post_sends_changed_votes_and_takes_what_it_left() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("7.json");
    let mut r = plugged();
    let now = Instant::now();
    let verified = |vote| ReviewLabel {
        name: "Verified".to_owned(),
        values: [(0, "No score"), (1, "Works")]
            .map(|(value, label)| LabelValue {
                value,
                label: label.to_owned(),
            })
            .to_vec(),
        vote,
    };
    let reviewed = Session {
        labels: vec![
            verified(0),
            ReviewLabel {
                name: "Code-Review".to_owned(),
                ..verified(0)
            },
        ],
        ..session(&file, HEAD)
    };
    r.start(("test".to_owned(), reviewed), None, now);
    r.job = None;
    let (form, _) = r.open_post(None);
    let mut form = form.expect("the dialog");
    form.labels[0].1 = 1;
    let post = Post {
        event: form.events[form.event].clone(),
        labels: &form.labels,
        summary: "Ship it\n",
        local: false,
    };
    let sent = r.send(None, post).expect("posted");
    let (id, request) = asked(&sent);
    assert_eq!(
        *request,
        Request::Post {
            id: "7".to_owned(),
            event: "post".to_owned(),
            labels: vec![("Verified".to_owned(), 1)],
            summary: Some("Ship it".to_owned()),
        }
    );
    r.said(
        (
            id,
            Said::Posted {
                posted: 0,
                failed: 0,
                pending: 0,
                url: Some("https://host/7".to_owned()),
                last_reviewed_head: Some(HEAD.to_owned()),
                labels: Some(vec![verified(1)]),
            },
        ),
        None,
        &shown(),
        now,
    );
    let done = r.said((id, Said::Done(Ok(()))), None, &shown(), now);
    assert_eq!(
        done,
        [
            Effect::Redraw,
            Effect::Say("review sent · https://host/7".to_owned()),
            Effect::Finished("Review sent · review 7".to_owned())
        ]
    );
    let mode = r.mode.as_ref().expect("the review");
    assert_eq!(mode.session.labels, [verified(1)]);
    assert_eq!(mode.session.last_reviewed_head.as_deref(), Some(HEAD));
    assert_eq!(
        r.status(None).as_deref().map(words).as_deref(),
        Some("review 7 · review sent · <OpenPosted>")
    );
}

/// A failed run says why, and the terminal hears of it, in the plugin's
/// words: they're escaped where they're drawn.
#[test]
fn a_failed_run_says_why() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("7.json");
    let mut r = plugged();
    let now = Instant::now();
    let (prepare, _) = r.start(("test".to_owned(), session(&file, HEAD)), None, now);
    let done = r.said(
        (prepare, Said::Done(Err("no \u{1b} access".to_owned()))),
        None,
        &shown(),
        now,
    );
    assert_eq!(
        done.last(),
        Some(&Effect::Fail("review 7: no \u{1b} access".to_owned()))
    );
}

/// Pushes about another review, or a head that's the session's, change
/// nothing; one about the review shown is drawn.
#[test]
fn pushes_about_the_review_shown_are_drawn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("7.json");
    let mut r = plugged();
    r.start(
        ("test".to_owned(), session(&file, HEAD)),
        None,
        Instant::now(),
    );
    r.job = None;
    let moved = |head: &str| Push::HeadMoved {
        id: "7".to_owned(),
        head: head.to_owned(),
    };
    assert_eq!(r.pushed(Push::CommentsChanged { id: "9".to_owned() }), []);
    assert_eq!(r.pushed(moved(HEAD)), [Effect::Redraw]);
    assert_eq!(
        r.status(None).as_deref().map(words).as_deref(),
        Some("review 7")
    );
    r.pushed(moved("cccc333"));
    assert_eq!(
        r.status(None).as_deref().map(words).as_deref(),
        Some("review 7 updated · new commits · <Update>")
    );
}

/// Reading the comments again plans the drafts again when they changed
/// elsewhere, not when only someone else's comment did, and says the
/// file's notice.
#[test]
fn a_reload_plans_again_when_the_drafts_changed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut comments = drafted(&dir);
    let path = dir.path().join("7.json");
    let mut r = plugged();
    let now = Instant::now();
    r.start(
        ("test".to_owned(), session(&path, HEAD)),
        Some(&mut comments),
        now,
    );
    r.mode.as_mut().expect("on a review").plans.due = None;
    let due = |r: &Reviewing| r.mode.as_ref().and_then(|m| m.plans.due);
    let write = |notice: &str, extra: &str| {
        let json = format!(
            r#"{{"version":1{notice},"comments":[
{{"id":"d1","path":"src/lib.rs","text":"Tidy this","author":"","created":"2026-11-17T10:00:00Z"}},{extra}
]}}"#
        );
        std::fs::write(&path, json).expect("write");
    };
    write(
        r#","notice":"read \u001b""#,
        r#"{"id":"p1","path":"src/lib.rs","text":"Posted","author":"ana","created":"2026-11-17T10:00:00Z","ref":"h:1"}"#,
    );
    assert_eq!(
        r.reload(&mut comments, Check::Bytes, now),
        [
            Effect::Changed(vec!["src/lib.rs".into()]),
            Effect::Say("read \u{1b}".to_owned())
        ]
    );
    assert_eq!(due(&r), None, "no draft changed");
    write(
        "",
        r#"{"id":"d2","path":"src/lib.rs","text":"Mine too","author":"","created":"2026-11-17T10:00:00Z"}"#,
    );
    assert_eq!(
        r.reload(&mut comments, Check::Bytes, now),
        [Effect::Changed(vec!["src/lib.rs".into()])]
    );
    assert!(due(&r).is_some());
}

/// Idle comes after a while without input on a review, told once; input
/// ends it.
#[test]
fn idle_is_told_once_and_input_ends_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut r = plugged();
    let now = Instant::now();
    assert_eq!(r.idle_deadline(now), None, "not on a review");
    r.start(
        ("test".to_owned(), session(&dir.path().join("7.json"), HEAD)),
        None,
        now,
    );
    let at = r.idle_deadline(now).expect("on a review");
    r.went_idle(now, now + Duration::from_secs(1));
    assert_eq!(r.take_idle(), None);
    r.went_idle(now, at);
    assert_eq!(r.take_idle(), Some(true));
    assert_eq!(r.idle_deadline(now), None, "idle already");
    r.went_idle(now, at);
    assert_eq!(r.take_idle(), None, "told once");
    r.input();
    assert_eq!(r.take_idle(), Some(false));
}

/// While a plugin runs, what it writes is read back as it says how far it
/// got, at most every quarter second, and at once as it ends; the file's
/// watch is held to the same pace while a run goes on, and read at once
/// otherwise.
#[test]
fn a_runs_writes_are_read_back_at_a_pace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut comments = drafted(&dir);
    let file = dir.path().join("7.json");
    let mut r = plugged();
    let t0 = Instant::now();
    let (prepare, _) = r.start(
        ("test".to_owned(), session(&file, HEAD)),
        Some(&mut comments),
        t0,
    );
    let write = |n: usize| {
        let op = format!(
            r#"[{{"op":"upsert","comment":{{"path":"src/lib.rs","text":"From the host {n}","author":"octocat","ref":"x:{n}"}}}}]"#
        );
        crate::review::apply_json(&file, op.as_bytes(), None).expect("written");
    };
    let changed = |effects: &[Effect]| matches!(effects.first(), Some(Effect::Changed(_)));
    let progress = |r: &mut Reviewing, comments: &mut Comments, at| {
        r.said(
            (prepare, Said::Progress("1/3".to_owned())),
            Some(comments),
            &shown(),
            at,
        )
    };
    write(1);
    assert!(
        changed(&progress(&mut r, &mut comments, t0)),
        "the first at once"
    );
    write(2);
    let soon = t0 + Duration::from_millis(100);
    assert!(!changed(&progress(&mut r, &mut comments, soon)));
    let due = r.reload_deadline().expect("due");
    assert_eq!(due, t0 + RELOAD_EVERY);
    assert!(
        r.due_reload(Some(&mut comments), soon).is_empty(),
        "not yet"
    );
    assert!(changed(&r.due_reload(Some(&mut comments), due)));
    assert_eq!(r.reload_deadline(), None);
    write(3);
    let watched = r.file_changed(&mut comments, due + Duration::from_millis(10));
    assert!(watched.is_empty(), "the watch waits too");
    assert!(r.reload_deadline().is_some());
    write(4);
    let done = r.said(
        (prepare, Said::Done(Ok(()))),
        Some(&mut comments),
        &shown(),
        due + Duration::from_millis(20),
    );
    assert!(changed(&done), "{done:?}");
    assert_eq!(r.reload_deadline(), None, "nothing left to read");
    assert_eq!(r.due_reload(Some(&mut comments), due + RELOAD_EVERY), []);
    write(5);
    assert!(
        changed(&r.file_changed(&mut comments, due + Duration::from_millis(30))),
        "no run"
    );
}

use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::*;

const WAIT: Duration = Duration::from_secs(10);

const SESSION: &str = r#"{"version":1,"session":{"id":"7","title":"Spell out five","url":"https://github.com/acme/widgets/pull/7","base":"a1b2c3d","head":"e4f5a6b","last_reviewed_head":null,"comments":"/data/7/comments.json"}}"#;

fn session() -> Session {
    Session {
        id: "7".to_owned(),
        label: None,
        title: Some("Spell out five".to_owned()),
        url: Some("https://github.com/acme/widgets/pull/7".to_owned()),
        base: "a1b2c3d".to_owned(),
        head: "e4f5a6b".to_owned(),
        last_reviewed_head: None,
        comments: PathBuf::from("/data/7/comments.json"),
    }
}

/// Runs `script` with `sh` as the plugin, in a group of its own as the
/// detached command would be, and collects what it says until it's done.
/// `while_running` gets the run once the first line is in.
fn run(script: &str, while_running: impl FnOnce(&Run)) -> Vec<Said> {
    let mut command = Command::new("sh");
    command.args(["-c", script]).process_group(0);
    let (tx, rx) = mpsc::channel();
    let run = start(command, "dffy-review-test", "post", move |said| {
        tx.send(said).is_ok()
    })
    .expect("starts");
    let mut said = vec![rx.recv_timeout(WAIT).expect("a line")];
    while_running(&run);
    while !matches!(said.last(), Some(Said::Done(_))) {
        said.push(rx.recv_timeout(WAIT).expect("the run ends"));
    }
    said
}

/// A describe answer names a command and two nouns; without the rest, a
/// review reads as the noun and its id, and posting it is one event. A
/// command that isn't a word, or a noun that isn't one printable line, is
/// refused, and so is an answer of another protocol or an error.
#[test]
fn a_describe_answer_is_checked() {
    let gitlab = r#"{"command":"mr","noun":"merge request","nouns":"merge requests","version":1}"#;
    let plain = Naming::decode(format!("{gitlab}\n").as_bytes()).expect("decodes");
    assert_eq!(
        plain,
        Naming {
            command: "mr".to_owned(),
            noun: "merge request".to_owned(),
            nouns: "merge requests".to_owned(),
            short: "merge request".to_owned(),
            id_prefix: String::new(),
            events: vec![ReviewEvent {
                id: "post".to_owned(),
                key: 'p',
                label: "post".to_owned(),
                done: "posted".to_owned(),
                submits: true,
            }],
            outdated_reply_warning: None,
        }
    );
    assert_eq!(plain.label("7"), "merge request 7");
    for (answer, why) in [
        (
            gitlab.replace("\"mr\"", "\"Mr\""),
            "isn't lowercase letters",
        ),
        (
            gitlab.replace("\"mr\"", "\"-mr\""),
            "isn't lowercase letters",
        ),
        (
            gitlab.replace("\"mr\"", "\"m r\""),
            "isn't lowercase letters",
        ),
        (
            gitlab.replace("merge requests", "merge\\u001b[2Jrequests"),
            "isn't 1 to 40 printable",
        ),
        (
            gitlab.replace("\"merge request\"", "\" \""),
            "isn't 1 to 40 printable",
        ),
        (
            gitlab.replace(",\"nouns\":\"merge requests\"", ""),
            "has no nouns",
        ),
        (gitlab.replace(":1}", ":2}"), "speaks review protocol 2"),
        (r#"{"version":1,"error":"no"}"#.to_owned(), "no"),
        (String::new(), "printed nothing"),
    ] {
        let refused = Naming::decode(answer.as_bytes()).expect_err(&answer);
        assert!(refused.contains(why), "{answer}: {refused}");
    }
}

/// A plugin names a review's short form and id prefix, the events the post
/// dialog offers, in order, and a warning for replies to outdated threads.
/// An event's key can't be one of the dialog's own, nor repeated.
#[test]
fn a_describe_answer_names_labels_and_events() {
    let full = r#"{"version":1,"command":"mr","noun":"merge request","nouns":"merge requests","short":"MR","id_prefix":"!","outdated_reply_warning":"easy to miss","events":[{"id":"draft","key":"d","label":"keep as draft","done":"kept","submits":false},{"id":"approve","key":"a","label":"approve","done":"approved"}]}"#;
    let naming = Naming::decode(full.as_bytes()).expect("decodes");
    assert_eq!(naming.label("12"), "MR !12");
    assert_eq!(
        naming.outdated_reply_warning.as_deref(),
        Some("easy to miss")
    );
    assert_eq!(
        naming.events,
        [
            ReviewEvent {
                id: "draft".to_owned(),
                key: 'd',
                label: "keep as draft".to_owned(),
                done: "kept".to_owned(),
                submits: false,
            },
            ReviewEvent {
                id: "approve".to_owned(),
                key: 'a',
                label: "approve".to_owned(),
                done: "approved".to_owned(),
                submits: true,
            },
        ]
    );
    for (answer, why) in [
        (
            full.replace(r#""key":"d""#, r#""key":"j""#),
            "other than j, k, s and v",
        ),
        (
            full.replace(r#""key":"d""#, r#""key":"a""#),
            "repeats an id or a key",
        ),
        (
            full.replace(r#""id":"draft""#, r#""id":"--x""#),
            "starting with a letter",
        ),
        (full.replace(r#","done":"kept""#, ""), "has no done"),
        (
            full.replace(r#""id_prefix":"!""#, r#""id_prefix":"\n""#),
            "id_prefix",
        ),
    ] {
        let refused = Naming::decode(answer.as_bytes()).expect_err(&answer);
        assert!(refused.contains(why), "{answer}: {refused}");
    }
}

#[test]
fn lines_decode_by_kind_and_unknown_ones_are_skipped() {
    let said = |line: &str| match decode(line.as_bytes()) {
        Ok(Some(Line::Said(said))) => said,
        other => panic!("{line}: {other:?}"),
    };
    assert_eq!(said(SESSION), Said::Session(session()));
    assert_eq!(
        said(r#"{"version":1,"progress":"100/312"}"#),
        Said::Progress("100/312".to_owned())
    );
    assert_eq!(
        said(r#"{"version":1,"head":"e4f5a6b","moved":true}"#),
        Said::Head {
            head: "e4f5a6b".to_owned(),
            moved: true
        }
    );
    assert_eq!(
        said(r#"{"version":1,"posted":11,"failed":1,"url":"https://x/pull/7#r"}"#),
        Said::Posted {
            posted: 11,
            failed: 1,
            held: false,
            held_message: None,
            url: Some("https://x/pull/7#r".to_owned())
        }
    );
    assert_eq!(
        said(
            r#"{"version":1,"posted":1,"failed":2,"held":true,"held_message":"review left pending","url":null}"#
        ),
        Said::Posted {
            posted: 1,
            failed: 2,
            held: true,
            held_message: Some("review left pending".to_owned()),
            url: None
        }
    );
    let draft = |id: &str, plan| {
        Said::Draft(Draft {
            id: id.to_owned(),
            path: "src/lib.rs".to_owned(),
            plan,
        })
    };
    assert_eq!(
        said(
            r#"{"version":1,"draft":{"id":"d1","path":"src/lib.rs","plan":"line","line":13,"side":"new"}}"#
        ),
        draft("d1", Plan::Line)
    );
    assert_eq!(
        said(r#"{"version":1,"draft":{"id":"d2","path":"src/lib.rs","plan":"file"}}"#),
        draft("d2", Plan::File)
    );
    assert_eq!(
        said(
            r#"{"version":1,"draft":{"id":"d3","path":"src/lib.rs","plan":"skip","reason":"file not in the PR"}}"#
        ),
        draft("d3", Plan::Skip("file not in the PR".to_owned()))
    );
    assert_eq!(
        said(r#"{"version":1,"planned":{"line":9,"file":1,"skip":2}}"#),
        Said::Planned {
            line: 9,
            file: 1,
            skip: 2
        }
    );
    assert_eq!(
        decode(br#"{"version":1,"error":"no such PR"}"#),
        Ok(Some(Line::Error("no such PR".to_owned())))
    );
    assert_eq!(decode(br#"{"version":1,"fetched":[7]}"#), Ok(None));
}

/// A review is named by a string id and, optionally, a label to show;
/// `list` may group one by the plugin's title, and `detect` answers with
/// a session or none.
#[test]
fn reviews_are_listed_and_detected_by_id() {
    let said = |line: &str| match decode(line.as_bytes()) {
        Ok(Some(Line::Said(said))) => said,
        other => panic!("{line}: {other:?}"),
    };
    assert_eq!(
        said(
            r#"{"version":1,"reviews":[{"id":"9","title":"Add a cache","author":"hubot","url":"u","head_ref":"cache","reason":"review_requested"},{"id":"I8f2c","label":"3","reason":"later","group":"Starred"}]}"#
        ),
        Said::Listed(vec![
            Listed {
                id: "9".to_owned(),
                label: None,
                title: "Add a cache".to_owned(),
                author: "hubot".to_owned(),
                reason: Reason::ReviewRequested,
                group: None,
            },
            Listed {
                id: "I8f2c".to_owned(),
                label: Some("3".to_owned()),
                title: String::new(),
                author: String::new(),
                reason: Reason::Other,
                group: Some("Starred".to_owned()),
            },
        ])
    );
    assert_eq!(
        said(&SESSION.replacen("\"session\"", "\"review\"", 1)),
        Said::Detected(Some(session()))
    );
    assert_eq!(said(r#"{"version":1,"review":null}"#), Said::Detected(None));
    // Words dffy draws as they are: a group or a held message that isn't
    // one printable line is dropped.
    let Said::Listed(listed) =
        said(r#"{"version":1,"reviews":[{"id":"1","group":"x\u001b[2Jy"}]}"#)
    else {
        panic!("a list");
    };
    assert_eq!(listed[0].group, None);
    assert!(matches!(
        said(r#"{"version":1,"posted":0,"failed":1,"held":true,"held_message":"a\nb"}"#),
        Said::Posted {
            held: true,
            held_message: None,
            ..
        }
    ));
}

/// Another version, or a session naming something other than commits and
/// an absolute file, stops the run.
#[test]
fn lines_dffy_cant_trust_are_refused() {
    let refused = |line: &str| decode(line.as_bytes()).expect_err(line);
    assert!(refused(r#"{"version":2,"progress":"1/2"}"#).contains("speaks review protocol 2"));
    assert!(refused("[1]").contains("isn't a JSON object"));
    assert!(refused(&SESSION.replace("a1b2c3d", "--output=x")).contains("base isn't a commit"));
    assert!(refused(&SESSION.replace("/data/7", "data/7")).contains("absolute"));
    for id in [r#""id":7"#, r#""id":"--output=x""#, r#""id":"""#] {
        let line = SESSION.replace(r#""id":"7""#, id);
        assert!(refused(&line).contains("no review id"), "{line}");
    }
}

/// An error after the session line leaves the session good: both arrive,
/// the error as the run's failure, over its exit status.
#[test]
fn a_session_then_an_error_says_both() {
    let script = format!(
        "echo '{SESSION}'; echo '{{\"version\":1,\"error\":\"threads incomplete\"}}'; exit 1"
    );
    assert_eq!(
        run(&script, |_| {}),
        [
            Said::Session(session()),
            Said::Done(Err("threads incomplete".to_owned()))
        ]
    );
}

#[test]
fn progress_lines_then_a_clean_exit() {
    let script = r#"echo '{"version":1,"progress":"1/2"}'; echo '{"version":1,"progress":"2/2"}'"#;
    assert_eq!(
        run(script, |_| {}),
        [
            Said::Progress("1/2".to_owned()),
            Said::Progress("2/2".to_owned()),
            Said::Done(Ok(()))
        ]
    );
}

/// Without an error line, the exit status says what went wrong, with the
/// last thing printed on stderr.
#[test]
fn an_exit_without_an_error_line_is_worded() {
    assert_eq!(
        run("exit 2", |_| {}),
        [Said::Done(Err(
            "dffy-review-test doesn't know post".to_owned()
        ))]
    );
    assert_eq!(
        run(
            "echo noise >&2; echo 'gh: not logged in' >&2; exit 1",
            |_| {}
        ),
        [Said::Done(Err(
            "dffy-review-test post failed with status 1: gh: not logged in".to_owned()
        ))]
    );
    let said = run(r#"echo '{"version":2,"progress":"1/2"}'; sleep 30"#, |_| {});
    assert!(
        matches!(&said[..], [Said::Done(Err(e))] if e.contains("speaks review protocol 2")),
        "{said:?}"
    );
}

/// A killed run still ends with its `Done`.
#[test]
fn a_killed_run_takes_what_it_started_along() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid = dir.path().join("pid");
    let script = format!(
        "sleep 30 & echo $! > '{}'; echo '{{\"version\":1,\"progress\":\"0/1\"}}'; wait",
        pid.display()
    );
    let started = Instant::now();
    let said = run(&script, Run::kill);
    assert!(started.elapsed() < WAIT, "it didn't wait for the sleep");
    assert_eq!(said[0], Said::Progress("0/1".to_owned()));
    assert!(
        matches!(&said[1], Said::Done(Err(e)) if e.contains("was killed")),
        "{said:?}"
    );
    let sleep: i32 = std::fs::read_to_string(&pid)
        .expect("pid")
        .trim()
        .parse()
        .expect("a pid");
    let sleep = Pid::from_raw(sleep).expect("a pid");
    let gone = Instant::now();
    while rustix::process::test_kill_process(sleep).is_ok() {
        assert!(gone.elapsed() < WAIT, "the sleep outlived the kill");
        std::thread::sleep(Duration::from_millis(10));
    }
}

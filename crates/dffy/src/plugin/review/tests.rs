use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::*;

const WAIT: Duration = Duration::from_secs(10);

const SESSION: &str = r#"{"version":1,"session":{"number":7,"title":"Spell out five","url":"https://github.com/acme/widgets/pull/7","base":"a1b2c3d","head":"e4f5a6b","last_reviewed_head":null,"comments":"/data/7/comments.json"}}"#;

fn session() -> Session {
    Session {
        number: 7,
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
            pending_review: false,
            url: Some("https://x/pull/7#r".to_owned())
        }
    );
    assert_eq!(
        said(r#"{"version":1,"posted":1,"failed":2,"pending_review":true,"url":null}"#),
        Said::Posted {
            posted: 1,
            failed: 2,
            pending_review: true,
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
        said(
            r#"{"version":1,"prs":[{"number":9,"title":"Add a cache","author":"hubot","url":"u","head_ref":"cache","reason":"review_requested"},{"number":3,"reason":"later"}]}"#
        ),
        Said::Prs(vec![
            Pr {
                number: 9,
                title: "Add a cache".to_owned(),
                author: "hubot".to_owned(),
                reason: Reason::ReviewRequested,
            },
            Pr {
                number: 3,
                title: String::new(),
                author: String::new(),
                reason: Reason::Other,
            },
        ])
    );
    assert_eq!(
        decode(br#"{"version":1,"error":"no such PR"}"#),
        Ok(Some(Line::Error("no such PR".to_owned())))
    );
    assert_eq!(decode(br#"{"version":1,"fetched":[7]}"#), Ok(None));
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

/// Killing a run kills what it started too, and the run still ends with
/// its `Done`.
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

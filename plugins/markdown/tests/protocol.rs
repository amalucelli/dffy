//! The built binary speaks dffy's renderer protocol: a request on stdin,
//! one answer on stdout, the exit status saying which kind it is.
#![expect(
    clippy::expect_used,
    reason = "a failed step fails the test where it broke"
)]

use std::io::Write as _;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn run(request: &str) -> (bool, Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dffy-render-markdown"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the renderer starts");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(request.as_bytes())
        .expect("request written");
    let out = child.wait_with_output().expect("the renderer exits");
    let answer = serde_json::from_slice(&out.stdout).expect("the answer is JSON");
    (out.status.success(), answer)
}

#[test]
fn a_request_round_trips_into_blocks_for_both_sides() {
    let request = json!({
        "version": 1,
        "path": "README.md",
        "old": "# Title\n\nOld text.\n",
        "new": null,
        "width": 60,
        "background": "light",
    });
    let (ok, answer) = run(&request.to_string());
    assert!(ok);
    assert_eq!(answer["version"], 1);
    assert_eq!(answer["new"], Value::Null);
    assert_eq!(
        answer["old"],
        json!([
            {"kind": "heading", "source": [0, 1], "lines": [[{"text": "Title", "style": ["heading"], "level": 1}]]},
            {"kind": "gap", "source": [1, 1], "lines": [[]]},
            {"kind": "paragraph", "source": [2, 3], "lines": [["Old text."]]},
            {"kind": "gap", "source": [3, 3], "lines": [[]]},
        ])
    );
}

#[test]
fn a_bad_request_gets_an_error_answer_and_a_failing_status() {
    let (ok, answer) = run("{\"version\": 9}");
    assert!(!ok);
    assert_eq!(answer["version"], 1);
    assert!(answer["error"].is_string());
}

#[test]
fn version_prints_the_release_version_without_reading_stdin() {
    let out = Command::new(env!("CARGO_BIN_EXE_dffy-render-markdown"))
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .expect("the renderer runs");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("dffy-render-markdown {}\n", env!("CARGO_PKG_VERSION"))
    );
}

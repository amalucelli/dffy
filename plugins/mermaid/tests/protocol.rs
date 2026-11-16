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
    let mut child = Command::new(env!("CARGO_BIN_EXE_dffy-render-mermaid"))
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
        "path": "flow.mmd",
        "old": "graph LR\n  A --> B\n",
        "new": null,
        "width": 60,
        "background": "dark",
    });
    let (ok, answer) = run(&request.to_string());
    assert!(ok);
    assert_eq!(answer["version"], 1);
    assert_eq!(answer["new"], Value::Null);
    let dim = |text: &str| json!({"text": text, "style": ["dim"]});
    let edge = |text: &str| json!({"text": text, "style": ["syntax-operator"]});
    assert_eq!(
        answer["old"],
        json!([{
            "kind": "flowchart",
            "source": [0, 2],
            "lines": [
                [dim("┌───┐"), "   ", dim("┌───┐")],
                [dim("│"), " A ", edge("├──▶"), dim("│"), " B ", dim("│")],
                [dim("└───┘"), "   ", dim("└───┘")],
            ],
        }])
    );
}

#[test]
fn a_sequence_diagram_is_a_block_per_statement() {
    let request = json!({
        "version": 1,
        "path": "seq.mmd",
        "old": null,
        "new": "sequenceDiagram\n  A->>B: hi\n  Note over A: hmm\n",
        "width": 60,
        "background": "dark",
    });
    let (ok, answer) = run(&request.to_string());
    assert!(ok);
    let blocks: Vec<(String, Value)> = answer["new"]
        .as_array()
        .expect("blocks")
        .iter()
        .map(|b| {
            (
                b["kind"].as_str().unwrap_or_default().to_owned(),
                b["source"].clone(),
            )
        })
        .collect();
    assert_eq!(
        blocks,
        [
            ("participants".to_owned(), json!([0, 1])),
            ("message".to_owned(), json!([1, 2])),
            ("note".to_owned(), json!([2, 3])),
        ]
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
    let out = Command::new(env!("CARGO_BIN_EXE_dffy-render-mermaid"))
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .expect("the renderer runs");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("dffy-render-mermaid {}\n", env!("CARGO_PKG_VERSION"))
    );
}

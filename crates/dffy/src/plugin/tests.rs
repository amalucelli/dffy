use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

#[test]
fn settings_map_extensions_to_names() {
    let renderers = Renderers::parse(" md:markdown .CSV:csv-2 ").expect("valid");
    assert_eq!(renderers.for_path(b"docs/README.md"), Some("markdown"));
    assert_eq!(renderers.for_path(b"data.Csv"), Some("csv-2"));
    assert_eq!(renderers.for_path(b"Makefile"), None);
    assert_eq!(renderers.for_path(b"dir.md/file"), None);
    assert_eq!(renderers.for_path(b".md"), None);
    assert_eq!(renderers.to_string(), "md:markdown csv:csv-2");
    assert!(Renderers::parse("").expect("empty").is_empty());
    for bad in [
        "markdown",
        "md:",
        ":markdown",
        "md:../evil",
        "md:Mark",
        "m/d:x",
    ] {
        assert!(Renderers::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn requests_carry_every_field_and_null_sides() {
    let request = Request {
        path: "a \"b\".md",
        old: None,
        new: Some("# hi\n"),
        width: 80,
        light: true,
    };
    let value = json::parse(request.encode().as_bytes()).expect("valid JSON");
    assert_eq!(
        value.get("version").and_then(Value::as_usize),
        Some(PROTOCOL)
    );
    assert_eq!(
        value.get("path").and_then(Value::as_str),
        Some("a \"b\".md")
    );
    assert_eq!(value.get("old"), Some(&Value::Null));
    assert_eq!(value.get("new").and_then(Value::as_str), Some("# hi\n"));
    assert_eq!(value.get("width").and_then(Value::as_usize), Some(80));
    assert_eq!(
        value.get("background").and_then(Value::as_str),
        Some("light")
    );
}

#[test]
fn answers_decode_into_blocks_with_styles() {
    let answer = br#"{"version": 1, "old": null, "new": [
        {"kind": "heading", "source": [0, 1], "lines": [[{"text": "Title", "style": ["heading"], "level": 1}]]},
        {"kind": "paragraph", "source": [2, 4], "lines": [["plain ", {"text": "bold", "style": ["bold", "future"]}, {"text": " k", "style": ["syntax-keyword"]}]]},
        {"kind": "code", "source": [4, 6], "language": "rust", "lines": [["fn main() {}"]]}
    ]}"#;
    let [old, new] = decode(answer, [None, Some(6)]).expect("valid");
    assert_eq!(old, Side::default());
    assert_eq!(new.blocks.len(), 3);
    let heading = &new.blocks[0].lines[0];
    assert_eq!(
        heading.paint,
        [Paint {
            start: 0,
            end: 5,
            syntax: Some(Syntax::Keyword),
            attributes: Style::BOLD | Style::UNDERLINE
        }]
    );
    let paragraph = &new.blocks[1];
    assert_eq!(paragraph.source, 2..4);
    assert_eq!(paragraph.lines[0].text, "plain bold k");
    assert_eq!(
        paragraph.lines[0].paint,
        [
            Paint {
                start: 6,
                end: 10,
                syntax: None,
                attributes: Style::BOLD
            },
            Paint {
                start: 10,
                end: 12,
                syntax: Some(Syntax::Keyword),
                attributes: 0
            }
        ]
    );
    assert_eq!(new.blocks[2].language.as_deref(), Some("rust"));
}

#[test]
fn answers_that_dont_fit_the_request_are_refused() {
    let cases: [(&[u8], &str); 8] = [
        (br#"{"version": 2, "new": []}"#, "speaks protocol 2"),
        (
            br#"{"version": 1, "old": [], "new": null}"#,
            "no blocks for new",
        ),
        (br#"{"new": []}"#, "no version"),
        (br#"{"version": 1, "error": "boom"}"#, "said: boom"),
        (b"[1]", "not a JSON object"),
        (
            br#"{"version": 1, "new": [{"kind": "p", "source": [0, 9], "lines": []}]}"#,
            "outside the side's 3 lines",
        ),
        (
            br#"{"version": 1, "new": [{"kind": "p", "source": [0, 1], "lines": [["a\nb"]]}]}"#,
            "line break",
        ),
        (b"not json", "bad answer"),
    ];
    for (answer, expected) in cases {
        let error = decode(answer, [None, Some(3)])
            .expect_err("refused")
            .to_string();
        assert!(error.contains(expected), "{error} lacks {expected}");
    }
}

/// An executable script named as a renderer in `dir`.
fn script(dir: &Path, name: &str, body: &str) -> Program {
    let path = dir.join(format!("{PREFIX}{name}"));
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    Program {
        name: name.to_owned(),
        path,
        stamp: (0, 0),
    }
}

const REQUEST: Request<'static> = Request {
    path: "a.md",
    old: Some("a\n"),
    new: Some("b\n"),
    width: 40,
    light: false,
};

const ANSWER: &str = r#"{"version":1,"old":[{"kind":"p","source":[0,1],"lines":[["a"]]}],"new":[{"kind":"p","source":[0,1],"lines":[["b"]]}]}"#;

#[test]
fn a_renderer_runs_and_its_answer_is_cached() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("ran");
    let program = script(
        dir.path(),
        "good",
        &format!(
            "cat > {input}\necho x >> {marker}\nprintf '%s' '{ANSWER}'",
            input = dir.path().join("input").display(),
            marker = marker.display()
        ),
    );
    let cache = Cache::new(dir.path().join("cache"), 1 << 20);
    for _ in 0..2 {
        let [old, new] =
            render(&program, &REQUEST, [1, 1], Some(&cache), || false).expect("rendered");
        assert_eq!(old.blocks[0].lines[0].text, "a");
        assert_eq!(new.blocks[0].lines[0].text, "b");
    }
    let input = std::fs::read_to_string(dir.path().join("input")).expect("request");
    assert_eq!(input, REQUEST.encode());
    assert_eq!(
        std::fs::read_to_string(marker).expect("marker"),
        "x\n",
        "the second render came from the cache"
    );
}

#[test]
fn failures_say_what_happened() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cases = [
        ("crash", "exit 3", "exited with status 3"),
        ("garbage", "echo nope", "bad answer"),
        (
            "says",
            r#"printf '{"version":1,"error":"no tables"}'; exit 1"#,
            "said: no tables",
        ),
        ("unread", &format!("printf '%s' '{ANSWER}'"), ""),
    ];
    for (name, body, expected) in cases {
        let program = script(dir.path(), name, body);
        match render(&program, &REQUEST, [1, 1], None, || false) {
            Ok(_) => assert!(expected.is_empty(), "{name} rendered"),
            Err(e) => assert!(
                !expected.is_empty() && e.to_string().contains(expected),
                "{name}: {e}"
            ),
        }
    }
}

#[test]
fn a_slow_renderer_is_killed_with_its_children() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid = dir.path().join("pid");
    let program = script(
        dir.path(),
        "slow",
        &format!("sleep 30 &\necho $! > {}\nwait", pid.display()),
    );
    let started = Instant::now();
    let result = render(&program, &REQUEST, [1, 1], None, || false);
    assert_eq!(result, Err(Failure::Timeout));
    // Well short of the sleep: a loaded machine may be slow to kill.
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(gone(&pid), "the renderer's child outlived it");

    let stop = AtomicBool::new(false);
    let result = render(&program, &REQUEST, [1, 1], None, || {
        stop.swap(true, Ordering::Relaxed)
    });
    assert_eq!(result, Err(Failure::Cancelled));
}

/// Whether the process whose id is in `pid_file` is gone, not just
/// orphaned, within 2s: signal 0 checks for it, which init reaps a moment
/// after the kill.
fn gone(pid_file: &Path) -> bool {
    let pid = std::fs::read_to_string(pid_file).expect("pid");
    let alive = || {
        Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .expect("kill runs")
            .success()
    };
    let waited = Instant::now();
    while alive() && waited.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive()
}

/// A renderer that answers and exits leaves nothing running behind it.
#[test]
fn what_a_renderer_started_is_killed_once_it_exits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid = dir.path().join("pid");
    let program = script(
        dir.path(),
        "helper",
        &format!(
            "sleep 30 </dev/null >/dev/null 2>&1 &\necho $! > {}\ncat >/dev/null\nprintf '%s' '{ANSWER}'",
            pid.display()
        ),
    );
    render(&program, &REQUEST, [1, 1], None, || false).expect("rendered");
    assert!(gone(&pid), "the renderer's helper outlived it");
}

/// `"error": null` is no error.
#[test]
fn a_null_error_is_no_error() {
    let answer = br#"{"version": 1, "error": null, "old": null, "new": []}"#;
    assert!(decode(answer, [None, Some(1)]).is_ok());
}

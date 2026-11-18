use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use dffy_core::diff::{Options, hunks};

use super::*;

/// 2024-02-29T12:34:56Z.
fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_709_210_096)
}

fn text(s: &str) -> Text {
    Text::new(s.as_bytes().to_vec())
}

fn file_in(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("dffy").join("comments.json")
}

/// Applies `ops`, given as the JSON `dffy comments apply` reads, to `c`.
fn ops(c: &mut Review, input: &str) -> Result<Vec<Option<String>>, String> {
    c.apply(parse_ops(input.as_bytes())?, now())
}

fn added(c: &mut Review, input: &str) -> String {
    ops(c, input).expect("applies")[0].clone().expect("an id")
}

const LINE_COMMENT: &str = r#"{"op":"add","comment":{"path":"src/lib.rs","side":"new","start":{"old":2,"new":2},"end":{"old":null,"new":3},"text":"Why?","anchor":{"before":["a"],"lines":["b","c"],"after":["d","e"]}}}"#;

const EVERY_FIELD: &str = r#"{"version":1,"notice":"as of 10:42","tool":{"pr":7},"comments":[
{"id":"a1","path":"new.rs","old_path":"old.rs","side":"new","start_side":"old","start":{"old":3,"new":null},"end":{"old":null,"new":5},"text":"Use \"checked_add\".\nIt\twraps.","suggestion":"x.checked_add(y)","author":"octocat","created":"2026-11-17T10:42:00Z","resolved":true,"ref":"github:PRRC_1","anchor":{"before":["fn f() {"],"lines":["    x + y","é \r"],"after":[]},"distance":2,"url":"https://github.com/o/r/pull/7#discussion_r1","outdated":true,"github_thread":"PRRT_1","pending":[1,{"a":null}]},
{"id":"a2","path":"new.rs","text":"Agreed.","author":"","created":"2026-11-17T11:00:00Z","in_reply_to":"a1","resolved":false}
]}
"#;

/// The file is the contract between dffy and integrations: what's read is
/// written back, fields dffy doesn't know included, at the top level and in
/// each comment.
#[test]
fn the_file_roundtrips_through_the_reader_and_writer_with_unknown_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = file_in(&dir);
    fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
    fs::write(&path, EVERY_FIELD).expect("write");

    let read_back = read(&path).expect("reads");
    let first = &read_back.comments[0];
    assert_eq!(
        (first.start, first.end),
        (
            Some(LineNumbers {
                old: Some(2),
                new: None
            }),
            Some(LineNumbers {
                old: None,
                new: Some(4)
            })
        ),
        "1-based in the file, 0-based in memory"
    );
    assert_eq!(first.start_side, Some(Side::Old));
    assert_eq!(first.reference.as_deref(), Some("github:PRRC_1"));
    assert_eq!(first.text, "Use \"checked_add\".\nIt\twraps.");
    assert_eq!(
        (
            first.url.as_deref(),
            first.outdated,
            read_back.comments[1].outdated
        ),
        (
            Some("https://github.com/o/r/pull/7#discussion_r1"),
            true,
            false
        )
    );
    assert_eq!(
        first
            .extra
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>(),
        ["github_thread", "pending"]
    );
    assert_eq!(read_back.extra[0].0, "tool");

    // A write through `apply` keeps everything it didn't change.
    apply(&path, vec![Op::Notice(Some("refreshed".to_owned()))]).expect("applies");
    let rewritten = read(&path).expect("reads");
    assert_eq!(rewritten.notice.as_deref(), Some("refreshed"));
    assert_eq!(rewritten.comments, read_back.comments);
    assert_eq!(rewritten.extra, read_back.extra);

    let bytes = fs::read(&path).expect("read");
    let value = json::parse(&bytes).expect("valid JSON");
    let comments = value
        .get("comments")
        .and_then(Value::as_array)
        .expect("comments");
    assert_eq!(
        comments[0].get("pending"),
        Some(&Value::Array(vec![
            Value::Number(1.0),
            Value::Object(vec![("a".to_owned(), Value::Null)])
        ]))
    );
    assert_eq!(
        comments[0].get("start"),
        json::parse(br#"{"old":3,"new":null}"#).ok().as_ref()
    );
    assert_eq!(
        comments[1].get("start"),
        None,
        "fields that don't apply are left out"
    );
    assert_eq!(comments[1].get("outdated"), None, "false is left out");
    assert_eq!(Review::parse(&bytes), Ok(rewritten));
}

#[test]
fn a_missing_or_empty_file_is_no_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(read(&file_in(&dir)), Ok(Review::default()));
    assert_eq!(Review::parse(b" \n"), Ok(Review::default()));
    assert_eq!(
        default_path(Path::new("/repo/.git")),
        Path::new("/repo/.git/dffy/comments.json")
    );
}

#[test]
fn unknown_versions_are_refused_and_never_rewritten() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("comments.json");
    let future = r#"{"version":2,"comments":[]}"#;
    fs::write(&path, future).expect("write");
    assert!(read(&path).is_err());
    assert!(apply(&path, vec![Op::Notice(None)]).is_err());
    assert_eq!(fs::read_to_string(&path).expect("read"), future);
    for bad in [
        &br#"{"comments":[]}"#[..],
        br#"{"version":1,"comments":[{"id":"a","path":"x","text":"t","resolved":"yes"}]}"#,
        br#"{"version":1,"comments":[{"path":"x","text":"t"}]}"#,
        b"[1]",
    ] {
        assert!(
            Review::parse(bad).is_err(),
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
}

#[test]
fn add_assigns_ids_and_defaults() {
    let mut c = Review::default();
    let ids = ops(
        &mut c,
        &format!(
            r#"[{LINE_COMMENT}, {{"op":"add","comment":{{"path":"README.md","text":"Typo."}}}}]"#
        ),
    )
    .expect("applies");
    assert_eq!(c.comments.len(), 2);
    assert_eq!(
        ids,
        [
            Some(c.comments[0].id.clone()),
            Some(c.comments[1].id.clone())
        ]
    );
    assert_ne!(c.comments[0].id, c.comments[1].id);
    let line = &c.comments[0];
    assert_eq!(line.created, "2024-02-29T12:34:56Z");
    assert_eq!((line.author.as_str(), line.resolved), ("", false));
    assert_eq!(line.range(), Some((Side::New, 1..3)));
    let file = &c.comments[1];
    assert_eq!((file.start, file.end, file.range()), (None, None, None));
}

#[test]
fn ids_are_not_reused_after_a_removal() {
    let mut c = Review::default();
    let first = added(&mut c, &format!("[{LINE_COMMENT}]"));
    ops(&mut c, &format!(r#"[{{"op":"rm","id":"{first}"}}]"#)).expect("removes");
    let second = added(&mut c, &format!("[{LINE_COMMENT}]"));
    assert_ne!(first, second);
}

#[test]
fn add_refuses_comments_missing_what_they_need() {
    for bad in [
        r#"{"op":"add","comment":{"id":"x","path":"a","text":"t"}}"#,
        r#"{"op":"add","comment":{"path":"a"}}"#,
        r#"{"op":"add","comment":{"text":"t"}}"#,
        r#"{"op":"add","comment":{"path":"a","text":"t","start":{"old":null,"new":1},"end":{"old":null,"new":1}}}"#,
        r#"{"op":"add","comment":{"path":"a","text":"t","end":{"old":null,"new":1},"anchor":{"lines":["x"]}}}"#,
        r#"{"op":"add","comment":{"path":"a","text":"t","side":"old","start":{"old":null,"new":1},"end":{"old":null,"new":1},"anchor":{"lines":["x"]}}}"#,
        r#"{"op":"add","comment":{"path":"a","text":"t","start":{"old":null,"new":1},"end":{"old":null,"new":1},"anchor":{"lines":[]}}}"#,
        r#"{"op":"add","comment":{"text":"t","in_reply_to":"nope"}}"#,
        r#"{"op":"add","comment":{"text":"t","in_reply_to":"x","in_reply_to_ref":"y"}}"#,
        r#"{"op":"frobnicate"}"#,
    ] {
        let mut c = Review::default();
        assert!(ops(&mut c, &format!("[{bad}]")).is_err(), "{bad}");
        assert_eq!(c, Review::default());
    }
}

/// A reply names any comment of its thread, by id or by ref, and joins the
/// thread's first comment.
#[test]
fn replies_join_their_threads_root() {
    let mut c = Review::default();
    let root = added(&mut c, &format!("[{LINE_COMMENT}]"));
    ops(
        &mut c,
        &format!(r#"[{{"op":"set","id":"{root}","fields":{{"ref":"github:R"}}}}]"#),
    )
    .expect("sets");
    let reply = added(
        &mut c,
        r#"[{"op":"add","comment":{"text":"Me too.","in_reply_to_ref":"github:R"}}]"#,
    );
    let nested = added(
        &mut c,
        &format!(r#"[{{"op":"add","comment":{{"text":"And me.","in_reply_to":"{reply}"}}}}]"#),
    );
    for id in [reply, nested] {
        let comment = c.comments.iter().find(|x| x.id == id).expect("added");
        assert_eq!(comment.in_reply_to.as_deref(), Some(root.as_str()));
        assert_eq!(comment.path, "src/lib.rs");
        assert_eq!((comment.end, &comment.anchor), (None, &None));
    }
}

#[test]
fn set_changes_fields_by_id_or_ref() {
    let mut c = Review::default();
    let id = added(&mut c, &format!("[{LINE_COMMENT}]"));
    let changed = ops(
        &mut c,
        &format!(
            r#"[{{"op":"set","id":"{id}","fields":{{"text":"Why not?","anchor":{{"lines":["z"]}}}}}},
                {{"op":"set","id":"{id}","fields":{{"ref":"github:1"}}}},
                {{"op":"set","ref":"github:1","fields":{{"resolved":true}}}}]"#
        ),
    )
    .expect("applies");
    assert_eq!(changed, [Some(id.clone()), Some(id.clone()), Some(id)]);
    let comment = &c.comments[0];
    assert_eq!(comment.text, "Why not?");
    assert_eq!(
        comment.anchor.as_ref().map(|a| a.lines.clone()),
        Some(vec!["z".to_owned()])
    );
    assert_eq!(comment.reference.as_deref(), Some("github:1"));
    assert!(comment.resolved);
    ops(
        &mut c,
        r#"[{"op":"set","ref":"github:1","fields":{"ref":null}}]"#,
    )
    .expect("clears");
    assert_eq!(c.comments[0].reference, None);
    for bad in [
        r#"{"op":"set","id":"missing","fields":{"resolved":true}}"#,
        r#"{"op":"set","ref":"missing","fields":{"resolved":true}}"#,
        r#"{"op":"set","fields":{"resolved":true}}"#,
        r#"{"op":"set","ref":"x","id":"y","fields":{"resolved":true}}"#,
        r#"{"op":"set","ref":"x","fields":{"path":"other.rs"}}"#,
    ] {
        assert!(ops(&mut c, &format!("[{bad}]")).is_err(), "{bad}");
    }
}

/// A comment with a ref is posted: its text is the host's, and only
/// `resolved`, `ref` and `anchor` may change.
#[test]
fn a_posted_comments_text_is_read_only() {
    let mut c = Review::default();
    let id = added(&mut c, &format!("[{LINE_COMMENT}]"));
    ops(
        &mut c,
        &format!(r#"[{{"op":"set","id":"{id}","fields":{{"ref":"github:1"}}}}]"#),
    )
    .expect("posts");
    let err = ops(
        &mut c,
        r#"[{"op":"set","ref":"github:1","fields":{"text":"edited"}}]"#,
    )
    .expect_err("read-only");
    assert!(err.starts_with("operation 0 (set): "), "{err}");
    assert_eq!(c.comments[0].text, "Why?");
    ops(
        &mut c,
        r#"[{"op":"set","ref":"github:1","fields":{"resolved":true,"anchor":{"lines":["y"]},"ref":"github:2"}}]"#,
    )
    .expect("the others change");
}

#[test]
fn refs_belong_to_one_comment() {
    let mut c = Review::default();
    let one = added(&mut c, &format!("[{LINE_COMMENT}]"));
    let two = added(&mut c, &format!("[{LINE_COMMENT}]"));
    ops(
        &mut c,
        &format!(r#"[{{"op":"set","id":"{one}","fields":{{"ref":"github:1"}}}}]"#),
    )
    .expect("sets");
    assert!(
        ops(
            &mut c,
            &format!(r#"[{{"op":"set","id":"{two}","fields":{{"ref":"github:1"}}}}]"#)
        )
        .is_err()
    );
    assert!(
        ops(
            &mut c,
            r#"[{"op":"add","comment":{"path":"a","text":"t","ref":"github:1"}}]"#
        )
        .is_err()
    );
}

/// Imports: by ref, the given fields replace the comment's, text included,
/// and the rest stay, its id and unknown fields among them.
#[test]
fn upsert_adds_or_updates_by_ref() {
    let mut c = Review::default();
    let root = r#"{"op":"upsert","comment":{"ref":"github:R","path":"a.rs","text":"v1","author":"octocat","created":"2026-01-01T00:00:00Z","github_thread":"T1"}}"#;
    let reply = r#"{"op":"upsert","comment":{"ref":"github:C","text":"reply","in_reply_to_ref":"github:R"}}"#;
    let ids = ops(&mut c, &format!("[{root}, {reply}]")).expect("adds");
    let root_id = ids[0].clone().expect("id");
    assert_eq!(c.comments[1].in_reply_to.as_ref(), Some(&root_id));
    assert_eq!(c.comments[1].path, "a.rs");

    ops(
        &mut c,
        &format!(r#"[{{"op":"set","id":"{root_id}","fields":{{"resolved":true}}}}]"#),
    )
    .expect("resolves");
    let again = ops(
        &mut c,
        r#"[{"op":"upsert","comment":{"ref":"github:R","text":"v2","suggestion":null}}]"#,
    )
    .expect("updates");
    assert_eq!(again, [Some(root_id.clone())]);
    assert_eq!(c.comments.len(), 2);
    let updated = &c.comments[0];
    assert_eq!(
        (
            updated.id.as_str(),
            updated.text.as_str(),
            updated.author.as_str()
        ),
        (root_id.as_str(), "v2", "octocat")
    );
    assert!(updated.resolved, "fields left out keep their values");
    assert_eq!(
        updated.extra,
        [("github_thread".to_owned(), Value::String("T1".to_owned()))]
    );

    for bad in [
        r#"{"op":"upsert","comment":{"path":"a.rs","text":"no ref"}}"#,
        r#"{"op":"upsert","comment":{"ref":"github:R","id":"x"}}"#,
        r#"{"op":"upsert","comment":{"ref":"github:R","text":null}}"#,
        r#"{"op":"upsert","comment":{"ref":"github:N","path":"a.rs"}}"#,
    ] {
        assert!(ops(&mut c, &format!("[{bad}]")).is_err(), "{bad}");
    }
}

#[test]
fn rm_takes_a_threads_replies_with_it() {
    let mut c = Review::default();
    let root = added(&mut c, &format!("[{LINE_COMMENT}]"));
    let other = added(&mut c, &format!("[{LINE_COMMENT}]"));
    ops(
        &mut c,
        &format!(
            r#"[{{"op":"add","comment":{{"text":"r","in_reply_to":"{root}"}}}},
                {{"op":"add","comment":{{"text":"r","in_reply_to":"{other}","ref":"github:X"}}}}]"#
        ),
    )
    .expect("replies");
    assert_eq!(
        ops(&mut c, &format!(r#"[{{"op":"rm","id":"{root}"}}]"#)).expect("removes"),
        [Some(root)]
    );
    assert_eq!(c.comments.len(), 2);
    ops(&mut c, r#"[{"op":"rm","ref":"github:X"}]"#).expect("removes");
    assert_eq!(
        c.comments.iter().map(|x| &x.id).collect::<Vec<_>>(),
        [&other]
    );
    assert!(ops(&mut c, r#"[{"op":"rm","id":"missing"}]"#).is_err());
}

#[test]
fn notice_sets_and_clears() {
    let mut c = Review::default();
    assert_eq!(
        ops(&mut c, r#"[{"op":"notice","notice":"stale"}]"#).expect("sets"),
        [None]
    );
    assert_eq!(c.notice.as_deref(), Some("stale"));
    ops(&mut c, r#"[{"op":"notice","notice":null}]"#).expect("clears");
    assert_eq!(c.notice, None);
}

/// A batch is applied whole or not at all, and the error names the
/// operation that failed.
#[test]
fn a_failed_batch_writes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = file_in(&dir);
    let input = format!(r#"[{LINE_COMMENT}, {{"op":"rm","id":"missing"}}]"#);
    let err = apply_json(&path, input.as_bytes()).expect_err("fails");
    assert!(err.starts_with("operation 1 (rm): "), "{err}");
    assert!(!path.exists());

    let mut c = Review::default();
    added(&mut c, &format!("[{LINE_COMMENT}]"));
    let before = c.clone();
    assert!(
        ops(
            &mut c,
            &format!(r#"[{LINE_COMMENT}, {{"op":"rm","id":"missing"}}]"#)
        )
        .is_err()
    );
    assert_eq!(c, before);
}

#[test]
fn apply_answers_an_id_per_operation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = file_in(&dir);
    let input = format!(r#"[{LINE_COMMENT}, {{"op":"notice","notice":"hi"}}]"#);
    let answer = apply_json(&path, input.as_bytes()).expect("applies");
    let value = json::parse(answer.as_bytes()).expect("valid JSON");
    let ids = value.get("ids").and_then(Value::as_array).expect("ids");
    let stored = read(&path).expect("reads");
    assert_eq!(
        ids,
        [Value::String(stored.comments[0].id.clone()), Value::Null]
    );
    assert!(path.with_file_name("comments.json.lock").exists());
}

#[test]
fn anchors_take_two_lines_of_context_without_terminators() {
    let t = text("one\ntwo\r\nthree\nfour\nfive\nsix");
    assert_eq!(
        Anchor::new(&t, 2..4),
        Anchor {
            before: vec!["one".to_owned(), "two\r".to_owned()],
            lines: vec!["three".to_owned(), "four".to_owned()],
            after: vec!["five".to_owned(), "six".to_owned()],
        }
    );
    let edge = Anchor::new(&t, 0..1);
    assert_eq!(edge.before, [""; 0]);
    assert_eq!(edge.after, ["two\r", "three"]);
    let lossy = Anchor::new(&Text::new(b"a\xff\n".to_vec()), 0..1);
    assert_eq!(lossy.lines, ["a\u{fffd}"]);
    assert_eq!(lossy.place(&Text::new(b"a\xff\n".to_vec()), 0), Some(0..1));
}

#[test]
fn comments_are_placed_where_their_anchor_is() {
    let original = text("a\nb\nx\ny\nc\nd\n");
    let anchor = Anchor::new(&original, 2..4);

    assert_eq!(anchor.place(&original, 2), Some(2..4), "exact");
    let moved = text("new\nnew\na\nb\nx\ny\nc\nd\n");
    assert_eq!(anchor.place(&moved, 2), Some(4..6), "moved, found once");
    let twice = text("a\nb\nx\ny\nc\nd\nz\na\nb\nx\ny\nc\nd\n");
    assert_eq!(anchor.place(&twice, 2), Some(2..4), "exact wins");
    assert_eq!(anchor.place(&twice, 3), None, "ambiguous");
    let edited = text("a\nb\nx\nY\nc\nd\n");
    assert_eq!(anchor.place(&edited, 2), None, "missing");
    let mut far = "pad\n".repeat(NEARBY + 10);
    far.push_str("a\nb\nx\ny\nc\nd\n");
    assert_eq!(anchor.place(&text(&far), 2), None, "too far away");
    assert_eq!(anchor.place(&text("a\n"), 5), None, "past the end");
}

/// GitHub's hunks have 3 lines of context, so distance decides whether a
/// host takes a comment on its lines.
#[test]
fn distance_counts_lines_to_the_nearest_change_on_the_side() {
    let old = text("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n");
    let added = text("1\n2\n3\n4\n5\nnew\n6\n7\n8\n9\n10\n");
    let h = hunks(&old, &added, &Options::default());
    assert_eq!(distance(5..6, Side::New, &h), Some(0));
    assert_eq!(distance(4..7, Side::New, &h), Some(0));
    assert_eq!(distance(1..2, Side::New, &h), Some(4));
    assert_eq!(distance(9..10, Side::New, &h), Some(4));
    // On the old side the addition sits between lines 5 and 6.
    assert_eq!(distance(4..5, Side::Old, &h), Some(1));
    assert_eq!(distance(5..6, Side::Old, &h), Some(1));
    assert_eq!(distance(1..2, Side::Old, &h), Some(4));

    let removed = text("1\n2\n3\n4\n6\n7\n8\n9\n10\n");
    let h = hunks(&old, &removed, &Options::default());
    assert_eq!(distance(4..5, Side::Old, &h), Some(0));
    assert_eq!(distance(3..4, Side::New, &h), Some(1));
    assert_eq!(distance(4..5, Side::New, &h), Some(1));
    assert_eq!(distance(0..1, Side::New, &h), Some(4));
    assert_eq!(distance(0..1, Side::New, &[]), None);
}

#[test]
fn a_range_across_sides_is_placed_on_its_end_side() {
    let mut c = Comment {
        side: Side::New,
        start_side: Some(Side::Old),
        start: Some(LineNumbers {
            old: Some(4),
            new: None,
        }),
        end: Some(LineNumbers {
            old: None,
            new: Some(6),
        }),
        ..Comment::default()
    };
    assert_eq!(c.range(), Some((Side::New, 6..7)));
    c.start = Some(LineNumbers {
        old: Some(3),
        new: Some(4),
    });
    assert_eq!(c.range(), Some((Side::New, 4..7)));
}

#[test]
fn timestamps_are_rfc3339_utc() {
    assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    assert_eq!(timestamp(now()), "2024-02-29T12:34:56Z");
    assert_eq!(
        timestamp(UNIX_EPOCH + Duration::from_secs(951_868_800)),
        "2000-03-01T00:00:00Z"
    );
}

/// The lock serializes writers: an apply waits while another holds it,
/// then lands on top of what that one wrote.
#[test]
fn writers_wait_for_the_lock_and_lose_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = file_in(&dir);
    apply(&path, vec![Op::Notice(None)]).expect("creates");

    let held = File::open(lock_path(&path)).expect("lock file");
    held.lock().expect("locks");
    let (tx, rx) = mpsc::channel();
    let waiting = {
        let path = path.clone();
        thread::spawn(move || {
            let result = apply_json(&path, format!("[{LINE_COMMENT}]").as_bytes());
            tx.send(()).expect("receiver alive");
            result
        })
    };
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "apply went ahead while the lock was held"
    );
    held.unlock().expect("unlocks");
    waiting.join().expect("joins").expect("applies");

    let writers: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            thread::spawn(move || apply_json(&path, format!("[{LINE_COMMENT}]").as_bytes()))
        })
        .collect();
    for w in writers {
        w.join().expect("joins").expect("applies");
    }
    assert_eq!(read(&path).expect("reads").comments.len(), 9);
}

/// Threads go by path and line, each with its replies, and a reply whose
/// thread is gone gets a heading of its own rather than joining the thread
/// before it.
#[test]
fn listing_heads_every_thread() {
    let review = Review::parse(
        br#"{"version":1,"comments":[
{"id":"z1","path":"z.rs","text":"Orphan.","author":"","created":"t3","in_reply_to":"gone","resolved":false},
{"id":"b1","path":"a.rs","side":"old","start":{"old":4,"new":null},"end":{"old":5,"new":null},"text":"Two\nlines.","suggestion":"fixed","author":"","created":"t1","resolved":true,"anchor":{"lines":["x","y"]}},
{"id":"a1","path":"a.rs","text":"File.","author":"ada","created":"t0","resolved":false},
{"id":"b2","path":"a.rs","text":"Reply.","author":"bob","created":"t2","in_reply_to":"b1","resolved":false,"ref":"r"}
]}"#,
    )
    .expect("parses");
    let mut c = Canvas::default();
    render(&threads(&review.comments), &mut c);
    let mut out = Vec::new();
    c.write_to(&mut out, None).expect("writes");
    insta::assert_snapshot!(String::from_utf8(out).expect("utf-8"), @r"
    a.rs
      a1 ada, t0
        File.

    a.rs:4-5 (old side), resolved
      b1 you, t1
        Two
        lines.
        suggestion:
          fixed
      b2 bob, t2, posted
        Reply.

    z.rs, reply in thread gone
      z1 you, t3
        Orphan.
    ");
}

/// Moving threads to another file adds each comment there with a new id,
/// replies under their moved root and the rest as they were, and takes
/// them out of the first file; a thread not asked for stays. When the
/// other file can't take them, neither file changes.
#[test]
fn moved_threads_leave_their_file_only_once_the_other_has_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (from, to) = (dir.path().join("local.json"), dir.path().join("pr.json"));
    let ids = apply(
        &from,
        parse_ops(format!("[{LINE_COMMENT},{LINE_COMMENT}]").as_bytes()).expect("ops"),
    )
    .expect("applies");
    let (moving, staying) = (ids[0].clone().expect("id"), ids[1].clone().expect("id"));
    apply(
        &from,
        vec![Op::Add {
            comment: Box::new(Comment {
                text: "And this.".to_owned(),
                ..Comment::default()
            }),
            reply_to: Some(Target::Id(moving.clone())),
        }],
    )
    .expect("replies");
    let before = read(&from).expect("reads");

    fs::write(&to, r#"{"version":2,"comments":[]}"#).expect("write");
    let refused = fs::read(&to).expect("read");
    assert!(move_threads(&from, &to, std::slice::from_ref(&moving)).is_err());
    assert_eq!(
        read(&from).expect("reads"),
        before,
        "the first file kept them"
    );
    assert_eq!(
        fs::read(&to).expect("read"),
        refused,
        "the other is as it was"
    );

    fs::write(
        &to,
        r#"{"version":1,"comments":[{"id":"pr1","path":"a.rs","text":"Posted","ref":"github:1"}]}"#,
    )
    .expect("write");
    assert_eq!(
        move_threads(&from, &to, std::slice::from_ref(&moving)),
        Ok(2)
    );
    let left = read(&from).expect("reads");
    assert_eq!(
        left.comments
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        [staying.as_str()]
    );
    let got = read(&to).expect("reads").comments;
    let root = &got[1];
    let old = &before.comments[0];
    assert_ne!(root.id, moving, "a new id");
    assert_eq!(
        (
            &root.text,
            &root.anchor,
            root.start,
            root.end,
            &root.created
        ),
        (&old.text, &old.anchor, old.start, old.end, &old.created)
    );
    assert_eq!(got[2].text, "And this.");
    assert_eq!(got[2].in_reply_to.as_ref(), Some(&root.id));
    assert_eq!(got[0].id, "pr1", "what was there stays");
}

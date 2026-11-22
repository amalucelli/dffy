use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use dffy_core::diff::{Options, Text, hunks};
use dffy_core::git::Repo;

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
    c.apply(parse_ops(input.as_bytes(), None)?, now())
}

/// Any review id is one file name in its plugin's directory, and two ids
/// are never one file.
#[test]
fn a_reviews_file_is_named_by_its_id() {
    let dir = Path::new("/r");
    let name = |id: &str| {
        review_file(dir, id)
            .strip_prefix(dir)
            .expect("in the directory")
            .to_str()
            .expect("utf-8")
            .to_owned()
    };
    assert_eq!(name("7"), "7.json");
    assert_eq!(name("i8f2c-1_v2.3"), "i8f2c-1_v2.3.json");
    assert_eq!(name("I8f2c"), "%498f2c.json");
    assert_eq!(name("../x"), "%2E.%2Fx.json");
    assert_eq!(name("a/b c"), "a%2Fb%20c.json");
    assert_ne!(name("a%2Fb"), name("a/b"));
    // A file system that ignores case, as macOS's, keeps them apart.
    assert_ne!(
        name("Ab").to_lowercase(),
        name("aB").to_lowercase(),
        "{} {}",
        name("Ab"),
        name("aB")
    );
}

/// An id too long for a file name, escaped, is cut, the id's digest
/// keeping two cut the same apart; the file and its lock can be written,
/// for an id that only just fits without its lock too.
#[test]
fn a_long_review_id_is_still_one_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let long = "Ab/".repeat(60);
    let (one, two) = (
        review_file(dir.path(), &long),
        review_file(dir.path(), &format!("{long}x")),
    );
    assert_ne!(one, two);
    assert_eq!(review_file(dir.path(), &long), one, "the same each time");
    let fits = "a".repeat(253 - ".json".len());
    for file in [one, two, review_file(dir.path(), &fits)] {
        let ops = format!("[{LINE_COMMENT}]");
        apply_json(&file, ops.as_bytes(), None).expect("written");
    }
}

/// Another comments file comes in, drafts and all, each comment with its
/// id: what the file has already, by id or by `ref`, stays as it is, a
/// reply joins its thread wherever the file has it, and importing again
/// adds nothing. A file of another version isn't taken.
#[test]
fn another_comments_file_is_imported_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (old, new) = (dir.path().join("old.json"), dir.path().join("r/7.json"));
    fs::write(
        &old,
        r#"{"version":1,"comments":[
{"id":"d1","path":"a.rs","text":"Draft","author":"","created":"2026-11-17T10:00:00Z"},
{"id":"h1","path":"a.rs","text":"Old words","author":"octocat","created":"2026-11-17T10:00:00Z","ref":"x:1"},
{"id":"d2","path":"a.rs","text":"My reply","author":"","created":"2026-11-17T10:00:00Z","in_reply_to":"h1"}
]}"#,
    )
    .expect("write");
    fs::create_dir_all(dir.path().join("r")).expect("mkdir");
    fs::write(
        &new,
        r#"{"version":1,"comments":[{"id":"n1","path":"a.rs","text":"Host words","author":"octocat","created":"2026-11-17T10:00:00Z","ref":"x:1"}]}"#,
    )
    .expect("write");
    assert_eq!(import(&old, &new).expect("imported"), 2);
    let got = read(&new).expect("read").comments;
    let summary: Vec<(&str, &str, Option<&str>)> = got
        .iter()
        .map(|c| (c.id.as_str(), c.text.as_str(), c.in_reply_to.as_deref()))
        .collect();
    assert_eq!(
        summary,
        [
            ("n1", "Host words", None),
            ("d1", "Draft", None),
            ("d2", "My reply", Some("n1")),
        ]
    );
    assert_eq!(import(&old, &new).expect("again"), 0, "once");
    assert_eq!(read(&new).expect("read").comments, got);
    fs::write(&old, "{\"version\":2,\"comments\":[]}").expect("write");
    assert!(import(&old, &new).is_err());
    assert!(import(&dir.path().join("none.json"), &new).is_err());
}

/// A writer that knows the branch gives it to the threads `add` starts
/// without one; `null` keeps a thread on no branch, and a reply takes its
/// thread's whatever it's given.
#[test]
fn add_takes_the_branch_given_for_threads_without_one() {
    let mut c = Review::default();
    let input = format!(
        r#"[{LINE_COMMENT},{{"op":"add","comment":{{"path":"a.rs","text":"x","branch":null}}}},{{"op":"add","comment":{{"path":"a.rs","text":"y","branch":"other"}}}},{{"op":"add","comment":{{"text":"z","in_reply_to":"none-yet"}}}}]"#
    );
    let parsed = parse_ops(input.as_bytes(), Some("feature/x")).expect("parses");
    let branches: Vec<Option<&str>> = parsed
        .iter()
        .map(|op| match op {
            Op::Add { comment, .. } => comment.branch.as_deref(),
            _ => panic!("an add"),
        })
        .collect();
    assert_eq!(
        branches,
        [Some("feature/x"), None, Some("other"), None],
        "a reply isn't given one: it takes its thread's"
    );
    let root = added(&mut c, &format!("[{LINE_COMMENT}]"));
    let reply = format!(r#"[{{"op":"add","comment":{{"text":"r","in_reply_to":"{root}"}}}}]"#);
    c.apply(
        parse_ops(reply.as_bytes(), Some("feature/x")).expect("parses"),
        now(),
    )
    .expect("applies");
    assert_eq!(c.comments[1].branch, None, "the thread's");
}

fn added(c: &mut Review, input: &str) -> String {
    ops(c, input).expect("applies")[0].clone().expect("an id")
}

const LINE_COMMENT: &str = r#"{"op":"add","comment":{"path":"src/lib.rs","side":"new","start":{"old":2,"new":2},"end":{"old":null,"new":3},"text":"Why?","anchor":{"before":["a"],"lines":["b","c"],"after":["d","e"]}}}"#;

const EVERY_FIELD: &str = r#"{"version":1,"notice":"as of 10:42","summary":"Two nits.","tool":{"pr":7},"comments":[
{"id":"a1","path":"new.rs","old_path":"old.rs","side":"new","start_side":"old","start":{"old":3,"new":null},"end":{"old":null,"new":5},"text":"Use \"checked_add\".\nIt\twraps.","suggestion":"x.checked_add(y)","author":"octocat","created":"2026-11-17T10:42:00Z","resolved":true,"ref":"github:PRRC_1","anchor":{"before":["fn f() {"],"lines":["    x + y","é \r"],"after":[]},"distance":2,"url":"https://github.com/o/r/pull/7#discussion_r1","outdated":true,"branch":"feature/x","pending":true,"post_error":"line 5 isn't in the diff","host_resolved":false,"github_thread":"PRRT_1","github_draft":[1,{"a":null}]},
{"id":"a2","path":"new.rs","text":"Agreed.","author":"","created":"2026-11-17T11:00:00Z","in_reply_to":"a1","resolved":false,"moved_from":"l9","applied":"2026-11-18T09:00:00Z"}
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
        ["github_thread", "github_draft"]
    );
    assert_eq!(
        (
            first.branch.as_deref(),
            first.pending,
            first.post_error.as_deref(),
            read_back.comments[1].pending
        ),
        (
            Some("feature/x"),
            true,
            Some("line 5 isn't in the diff"),
            false
        )
    );
    assert_eq!(
        (first.host_resolved, read_back.comments[1].host_resolved),
        (Some(false), None)
    );
    assert_eq!(read_back.summary.as_deref(), Some("Two nits."));
    assert_eq!(read_back.extra[0].0, "tool");
    assert_eq!(read_back.comments[1].moved_from.as_deref(), Some("l9"));
    assert_eq!(
        read_back.comments[1].applied.as_deref(),
        Some("2026-11-18T09:00:00Z")
    );
    assert!(read_back.comments[1].extra.is_empty(), "fields dffy knows");

    // A write through `apply` keeps everything it didn't change.
    apply(&path, vec![Op::Notice(Some("refreshed".to_owned()))]).expect("applies");
    let rewritten = read(&path).expect("reads");
    assert_eq!(rewritten.notice.as_deref(), Some("refreshed"));
    assert_eq!(rewritten.comments, read_back.comments);
    assert_eq!(rewritten.extra, read_back.extra);
    assert_eq!(rewritten.summary, read_back.summary);

    let bytes = fs::read(&path).expect("read");
    let value = json::parse(&bytes).expect("valid JSON");
    let comments = value
        .get("comments")
        .and_then(Value::as_array)
        .expect("comments");
    assert_eq!(
        comments[0].get("github_draft"),
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
    assert_eq!(comments[1].get("pending"), None, "false is left out");
    assert_eq!(Review::parse(&bytes), Ok(rewritten));
}

#[test]
fn a_missing_or_empty_file_is_no_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(read(&file_in(&dir)), Ok(Review::default()));
    assert_eq!(Review::parse(b" \n"), Ok(Review::default()));
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

/// After a suggestion is applied: its thread moves to the lines it put
/// there, a range from the old side becoming one on the new, and its
/// comment is marked applied, posted or not; a new suggestion clears the
/// mark. Lines move only on a line comment, and only together.
#[test]
fn set_moves_a_threads_lines_and_marks_a_suggestion_applied() {
    let mut c = Review::default();
    let id = added(&mut c, &format!("[{LINE_COMMENT}]"));
    let reply = added(
        &mut c,
        &format!(r#"[{{"op":"add","comment":{{"text":"r","in_reply_to":"{id}"}}}}]"#),
    );
    ops(
        &mut c,
        &format!(
            r#"[{{"op":"set","id":"{id}","fields":{{"ref":"github:1"}}}},
                {{"op":"set","id":"{id}","fields":{{"start":{{"old":null,"new":7}},"end":{{"old":null,"new":8}},"start_side":null,"anchor":{{"lines":["x","y"]}}}}}},
                {{"op":"set","id":"{id}","fields":{{"applied":"2026-11-18T09:00:00Z"}}}}]"#
        ),
    )
    .expect("applies to a posted comment too");
    let root = &c.comments[0];
    assert_eq!(
        (root.range(), root.start_side, root.applied.as_deref()),
        (Some((Side::New, 6..8)), None, Some("2026-11-18T09:00:00Z"))
    );
    ops(
        &mut c,
        &format!(r#"[{{"op":"set","id":"{reply}","fields":{{"applied":"t"}}}}]"#),
    )
    .expect("marks a reply");
    ops(
        &mut c,
        &format!(r#"[{{"op":"set","id":"{reply}","fields":{{"suggestion":"z"}}}}]"#),
    )
    .expect("suggests again");
    assert_eq!(c.comments[1].applied, None, "a new suggestion to apply");
    for bad in [
        format!(
            r#"{{"op":"set","id":"{reply}","fields":{{"start":{{"new":1}},"end":{{"new":1}}}}}}"#
        ),
        format!(r#"{{"op":"set","id":"{id}","fields":{{"start":{{"new":1}}}}}}"#),
    ] {
        assert!(ops(&mut c, &format!("[{bad}]")).is_err(), "{bad}");
    }
}

/// A comment with a ref is posted: its text is the host's, and only
/// `resolved` and `anchor` may change, and `ref` only to none.
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
        r#"[{"op":"set","ref":"github:1","fields":{"resolved":true,"anchor":{"lines":["y"]}}}]"#,
    )
    .expect("the others change");
}

/// The text is the host's only while the comment stays posted through the
/// set: clearing the ref frees it in the same op, changing it doesn't.
#[test]
fn a_set_checks_the_text_against_the_ref_it_leaves() {
    let mut c = Review::default();
    let id = added(&mut c, &format!("[{LINE_COMMENT}]"));
    ops(
        &mut c,
        &format!(r#"[{{"op":"set","id":"{id}","fields":{{"ref":"github:1","text":"Posted."}}}}]"#),
    )
    .expect("posts with its text");
    assert!(
        ops(
            &mut c,
            r#"[{"op":"set","ref":"github:1","fields":{"ref":"github:2","text":"edited"}}]"#,
        )
        .is_err()
    );
    ops(
        &mut c,
        r#"[{"op":"set","ref":"github:1","fields":{"ref":null,"text":"edited"}}]"#,
    )
    .expect("unposts and edits");
    let comment = &c.comments[0];
    assert_eq!(
        (comment.reference.as_deref(), comment.text.as_str()),
        (None, "edited")
    );
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

/// An upsert moves a comment into another thread only when nothing is
/// lost: not a first comment that replies answer, which would leave them
/// under no thread, nor into its own thread.
#[test]
fn an_upsert_into_a_thread_keeps_every_reply_under_one() {
    let mut c = Review::default();
    ops(
        &mut c,
        r#"[{"op":"upsert","comment":{"ref":"github:R","path":"a.rs","text":"root"}},
            {"op":"upsert","comment":{"ref":"github:C","text":"reply","in_reply_to_ref":"github:R"}},
            {"op":"upsert","comment":{"ref":"github:S","path":"a.rs","text":"other"}}]"#,
    )
    .expect("adds");
    let before = c.clone();
    for bad in [
        r#"{"op":"upsert","comment":{"ref":"github:R","in_reply_to_ref":"github:S"}}"#,
        r#"{"op":"upsert","comment":{"ref":"github:S","in_reply_to_ref":"github:S"}}"#,
    ] {
        assert!(ops(&mut c, &format!("[{bad}]")).is_err(), "{bad}");
        assert_eq!(c, before);
    }
    ops(
        &mut c,
        r#"[{"op":"upsert","comment":{"ref":"github:S","in_reply_to_ref":"github:C"}}]"#,
    )
    .expect("a first comment without replies joins a thread");
    assert_eq!(c.comments[2].in_reply_to, Some(c.comments[0].id.clone()));
}

/// A `set` leaves a comment the file's format allows, as `add` and
/// `upsert` do: a general comment keeps its ref, and only a line comment
/// has an anchor.
#[test]
fn a_set_keeps_the_comment_valid() {
    let mut c = Review::default();
    ops(
        &mut c,
        r#"[{"op":"upsert","comment":{"ref":"github:G","text":"description"}},
            {"op":"add","comment":{"path":"a.rs","text":"on the file"}}]"#,
    )
    .expect("adds");
    let file = c.comments[1].id.clone();
    let reply = added(
        &mut c,
        &format!(r#"[{{"op":"add","comment":{{"text":"reply","in_reply_to":"{file}"}}}}]"#),
    );
    let before = c.clone();
    for bad in [
        r#"{"op":"set","ref":"github:G","fields":{"ref":null}}"#.to_owned(),
        format!(r#"{{"op":"set","id":"{file}","fields":{{"anchor":{{"lines":["x"]}}}}}}"#),
        format!(r#"{{"op":"set","id":"{reply}","fields":{{"anchor":{{"lines":["x"]}}}}}}"#),
    ] {
        assert!(ops(&mut c, &format!("[{bad}]")).is_err(), "{bad}");
        assert_eq!(c, before);
    }
}

/// A general comment is on the review rather than a file, such as its
/// description: no `path` and no lines. Its `kind` and `label` are the
/// integration's words, kept as given. It's written without a `path`, and
/// reads back the same.
#[test]
fn general_comments_roundtrip_without_a_path() {
    let mut c = Review::default();
    ops(
        &mut c,
        r#"[{"op":"upsert","comment":{"ref":"github:PR_1","text":"Spells out five.","author":"octocat","created":"2026-11-15T08:00:00Z","kind":"description"}},
            {"op":"upsert","comment":{"ref":"github:PRR_1","text":"Looks good.","author":"hubot","created":"2026-11-16T09:00:00Z","kind":"review","label":"approved"}}]"#,
    )
    .expect("applies");
    let review = &c.comments[1];
    assert_eq!(
        (
            review.path.as_str(),
            review.kind.as_deref(),
            review.label.as_deref(),
            review.range()
        ),
        ("", Some("review"), Some("approved"), None)
    );
    let encoded = c.encode();
    let value = json::parse(encoded.as_bytes()).expect("valid JSON");
    let written = value
        .get("comments")
        .and_then(Value::as_array)
        .expect("comments");
    assert_eq!(written[0].get("path"), None, "left out");
    assert_eq!(
        written[1].get("label"),
        Some(&Value::String("approved".to_owned()))
    );
    assert_eq!(Review::parse(encoded.as_bytes()), Ok(c.clone()));

    // An update keeps it general; one without a ref, or with lines, isn't
    // one.
    ops(
        &mut c,
        r#"[{"op":"upsert","comment":{"ref":"github:PRR_1","text":"Looks good!","label":"changes requested"}}]"#,
    )
    .expect("updates");
    assert_eq!(c.comments[1].label.as_deref(), Some("changes requested"));
    for bad in [
        r#"{"op":"add","comment":{"text":"t","kind":"comment"}}"#,
        r#"{"op":"upsert","comment":{"ref":"github:L","text":"t","start":{"old":null,"new":1},"end":{"old":null,"new":1},"anchor":{"lines":["x"]}}}"#,
    ] {
        assert!(ops(&mut c, &format!("[{bad}]")).is_err(), "{bad}");
    }
}

/// `dffy comments` heads a general comment with the conversation, its kind
/// and its label, before the threads on files.
#[test]
fn listing_heads_general_comments_as_the_conversation() {
    let review = Review::parse(
        br#"{"version":1,"comments":[
{"id":"a1","path":"a.rs","text":"File.","author":"ada","created":"t0","resolved":false},
{"id":"g1","text":"Looks good.","author":"hubot","created":"t1","resolved":false,"ref":"r1","kind":"review","label":"approved"},
{"id":"g2","text":"Thanks!","author":"","created":"t2","resolved":false,"ref":"r2"}
]}"#,
    )
    .expect("parses");
    let mut c = crate::render::Canvas::default();
    crate::print::comments(&threads(&review.comments), None, &[], &mut c);
    let mut out = Vec::new();
    c.write_to(&mut out, None).expect("writes");
    insta::assert_snapshot!(String::from_utf8(out).expect("utf-8"), @r"
    conversation, review, approved
      g1 hubot, t1, posted
        Looks good.

    conversation
      g2 you, t2, posted
        Thanks!

    a.rs
      a1 ada, t0
        File.
    ");
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
    assert_eq!(
        ops(&mut c, r#"[{"op":"summary","summary":"Ship it."}]"#).expect("sets"),
        [None]
    );
    assert_eq!(c.summary.as_deref(), Some("Ship it."));
    ops(&mut c, r#"[{"op":"summary","summary":null}]"#).expect("clears");
    assert_eq!(c.summary, None);
}

fn comment<'a>(c: &'a Review, id: &str) -> &'a Comment {
    c.comments.iter().find(|x| x.id == id).expect("in the file")
}

/// Beside the text, `set` adopts a comment as yours (author only to
/// empty), changes its suggestion, moves its thread to another branch
/// (from any of its comments), and marks how a post went.
#[test]
fn set_adopts_retargets_and_marks_posts() {
    let mut c = Review::default();
    let root = added(
        &mut c,
        r#"[{"op":"add","comment":{"path":"a.rs","text":"Rename it.","author":"claude","branch":"topic"}}]"#,
    );
    let reply = added(
        &mut c,
        &format!(
            r#"[{{"op":"add","comment":{{"text":"Yes.","in_reply_to":"{root}","branch":"elsewhere"}}}}]"#
        ),
    );
    let branches = |c: &Review| {
        c.comments
            .iter()
            .map(|x| x.branch.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        branches(&c),
        [Some("topic".to_owned()), Some("topic".to_owned())],
        "a reply is on its thread's branch"
    );
    ops(
        &mut c,
        &format!(
            r#"[{{"op":"set","id":"{root}","fields":{{"author":"","suggestion":"fn renamed()","pending":true,"post_error":"not in the diff"}}}},
                {{"op":"set","id":"{reply}","fields":{{"branch":"renamed"}}}}]"#
        ),
    )
    .expect("applies");
    let r = comment(&c, &root);
    assert_eq!(
        (
            r.author.as_str(),
            r.suggestion.as_deref(),
            r.pending,
            r.post_error.as_deref()
        ),
        ("", Some("fn renamed()"), true, Some("not in the diff"))
    );
    assert_eq!(
        branches(&c),
        [Some("renamed".to_owned()), Some("renamed".to_owned())]
    );
    ops(
        &mut c,
        &format!(
            r#"[{{"op":"set","id":"{root}","fields":{{"suggestion":null,"pending":false,"post_error":null,"branch":null}}}}]"#
        ),
    )
    .expect("clears");
    let r = comment(&c, &root);
    assert_eq!(
        (r.suggestion.as_deref(), r.pending, r.post_error.as_deref()),
        (None, false, None)
    );
    assert_eq!(branches(&c), [None, None]);
    for bad in [
        r#"{"author":"bob"}"#,
        r#"{"pending":"yes"}"#,
        r#"{"branch":1}"#,
    ] {
        let op = format!(r#"[{{"op":"set","id":"{root}","fields":{bad}}}]"#);
        assert!(ops(&mut c, &op).is_err(), "{bad}");
    }

    // Posted, its suggestion and author are the host's, as its text is.
    ops(
        &mut c,
        &format!(r#"[{{"op":"set","id":"{reply}","fields":{{"ref":"github:1"}}}}]"#),
    )
    .expect("posts");
    for posted in [r#"{"suggestion":"x"}"#, r#"{"author":""}"#] {
        let op = format!(r#"[{{"op":"set","ref":"github:1","fields":{posted}}}]"#);
        assert!(ops(&mut c, &op).is_err(), "{posted}");
    }
}

/// A selection keeps whole threads, judged by their first comment: a reply
/// is open or resolved, on a file and on a branch as its thread is.
#[test]
fn selections_keep_whole_threads() {
    let review = Review::parse(
        br#"{"version":1,"comments":[
{"id":"g1","text":"Ship it.","author":"hubot","created":"t","resolved":false,"ref":"r1"},
{"id":"a1","path":"a.rs","text":"Open.","author":"","created":"t","resolved":false,"branch":"topic"},
{"id":"a2","path":"a.rs","text":"Reply.","author":"","created":"t","resolved":false,"in_reply_to":"a1"},
{"id":"b1","path":"b.rs","old_path":"old_b.rs","text":"Done.","author":"","created":"t","resolved":true},
{"id":"b2","path":"b.rs","old_path":"old_b.rs","text":"Reply.","author":"","created":"t","resolved":false,"in_reply_to":"b1"},
{"id":"c1","path":"c.rs","text":"Elsewhere.","author":"","created":"t","resolved":true,"branch":"main"}
]}"#,
    )
    .expect("parses");
    let ids = |s: Select| {
        review
            .selected(&s)
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>()
    };
    let all = Select::default();
    assert_eq!(ids(all), ["g1", "a1", "a2", "b1", "b2", "c1"]);
    let resolved = |resolved| Select {
        resolved: Some(resolved),
        ..all
    };
    assert_eq!(ids(resolved(true)), ["b1", "b2", "c1"]);
    assert_eq!(ids(resolved(false)), ["g1", "a1", "a2"]);
    assert_eq!(
        ids(Select {
            path: Some("old_b.rs"),
            ..all
        }),
        ["b1", "b2"]
    );
    let on = |branch| Select {
        branches: Branches::On(branch),
        ..all
    };
    assert_eq!(ids(on(Some("topic"))), ["g1", "a1", "a2", "b1", "b2"]);
    assert_eq!(ids(on(Some("main"))), ["g1", "b1", "b2", "c1"]);
    assert_eq!(ids(on(None)), ["g1", "b1", "b2"], "detached: no branch's");
}

/// `dffy comments rm --resolved`: the resolved threads a selection keeps
/// go whole, under the lock, and the rest stay as they were.
#[test]
fn removing_resolved_threads_takes_their_replies() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = file_in(&dir);
    fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
    fs::write(
        &path,
        r#"{"version":1,"notice":"kept","comments":[
{"id":"a1","path":"a.rs","text":"Open.","author":"","created":"t","resolved":false},
{"id":"b1","path":"b.rs","text":"Done.","author":"","created":"t","resolved":true},
{"id":"b2","path":"b.rs","text":"Reply.","author":"","created":"t","resolved":false,"in_reply_to":"b1"},
{"id":"c1","path":"c.rs","text":"Elsewhere.","author":"","created":"t","resolved":true,"branch":"main"}
]}"#,
    )
    .expect("write");
    let resolved_here = Select {
        resolved: Some(true),
        branches: Branches::On(Some("topic")),
        ..Select::default()
    };
    assert_eq!(remove_threads(&path, &resolved_here), Ok(2));
    let left = read(&path).expect("reads");
    assert_eq!(
        left.comments
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["a1", "c1"]
    );
    assert_eq!(left.notice.as_deref(), Some("kept"));
}

/// `dffy comments --markdown`: each thread under a heading with its place,
/// over the commented lines in a fence longer than any run of backticks in
/// them, then each comment with its suggestion.
#[test]
fn markdown_exports_threads_with_their_lines() {
    let review = Review::parse(
        br#"{"version":1,"summary":"Two nits.","comments":[
{"id":"b1","path":"src/lib.rs","side":"old","start":{"old":4,"new":null},"end":{"old":5,"new":null},"text":"Use **checked** math.","suggestion":"a.checked_add(b)","author":"","created":"t1","resolved":true,"anchor":{"before":["x"],"lines":["let s = ```;","a + b"],"after":[]}},
{"id":"b2","path":"src/lib.rs","text":"Done.","author":"octocat","created":"t2","in_reply_to":"b1","resolved":false},
{"id":"a1","path":"README.md","text":"Say more.","author":"ada","created":"t0","resolved":false},
{"id":"g1","text":"Looks good.","author":"hubot","created":"t3","resolved":false,"ref":"r1","kind":"review","label":"approved"}
]}"#,
    )
    .expect("parses");
    insta::assert_snapshot!(markdown(&threads(&review.comments), review.summary.as_deref()), @r"
    ## Summary

    Two nits.

    ## Conversation, review, approved

    **hubot**, t3

    Looks good.

    ## `README.md`

    **ada**, t0

    Say more.

    ## `src/lib.rs`, lines 4-5 (old side), resolved

    ````
    let s = ```;
    a + b
    ````

    **you**, t1

    Use **checked** math.

    Suggested change:

    ```
    a.checked_add(b)
    ```

    **octocat**, t2

    Done.
    ");
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "A")
        .env("GIT_COMMITTER_EMAIL", "a@example.com")
        .output()
        .expect("git runs")
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit(dir: &Path, name: &str, contents: &str) {
    fs::write(dir.join(name), contents).expect("write");
    git_ok(dir, &["add", "-A"]);
    git_ok(dir, &["commit", "-q", "-m", name]);
}

/// A repository on `main` with one commit, in a canonical path, so paths
/// git and dffy give compare equal.
fn repository() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path().canonicalize().expect("canonical");
    git_ok(&d, &["init", "-q", "-b", "main"]);
    commit(&d, "a.txt", "base\n");
    (dir, d)
}

/// Local comments are kept per branch: HEAD's, an unborn one's, the
/// branch being rebased while a rebase stops, and none on a detached HEAD.
#[test]
fn the_current_branch_follows_head_and_rebases() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git_ok(d, &["init", "-q", "-b", "main"]);
    let branch = || current_branch(&Repo::discover(d).expect("repo"));
    assert_eq!(branch().as_deref(), Some("main"), "unborn");
    commit(d, "a.txt", "base\n");
    git_ok(d, &["checkout", "-q", "-b", "feature/x"]);
    commit(d, "a.txt", "feature\n");
    assert_eq!(branch().as_deref(), Some("feature/x"));
    git_ok(d, &["checkout", "-q", "main"]);
    commit(d, "a.txt", "main\n");
    git_ok(d, &["checkout", "-q", "feature/x"]);
    for backend in ["--merge", "--apply"] {
        assert!(
            !git(d, &["rebase", backend, "main"]).status.success(),
            "{backend} stops on the conflict"
        );
        assert_eq!(branch().as_deref(), Some("feature/x"), "{backend}");
        git_ok(d, &["rebase", "--abort"]);
    }
    git_ok(d, &["checkout", "-q", "--detach"]);
    assert_eq!(branch(), None);
}

/// The local review's file is in the git directory every worktree shares.
/// A linked worktree's file from before moves there once, and stays where
/// it is when the shared one already exists; until it moves, read-only
/// readers find it where it was.
#[test]
fn a_worktrees_old_file_moves_to_the_shared_git_directory() {
    let (_dir, d) = repository();
    let shared = d.join(".git/dffy/comments.json");
    assert_eq!(
        local_file(&Repo::discover(&d).expect("repo")),
        Ok(Some(shared.clone()))
    );
    let old_file = |name: &str| {
        let linked = d.join(name);
        git_ok(
            &d,
            &["worktree", "add", "-q", linked.to_str().expect("utf-8")],
        );
        let repo = Repo::discover(&linked).expect("repo");
        let old = repo
            .git_dir()
            .expect("a git dir")
            .canonicalize()
            .expect("exists")
            .join("dffy/comments.json");
        apply(
            &old,
            parse_ops(format!("[{LINE_COMMENT}]").as_bytes(), None).expect("ops"),
        )
        .expect("writes");
        (repo, old)
    };
    let (linked, old) = old_file("linked");
    let before = read(&old).expect("reads");
    assert_eq!(local_file_read_only(&linked), Some(old.clone()));
    assert_eq!(local_file(&linked), Ok(Some(shared.clone())));
    assert!(!old.exists(), "moved");
    assert_eq!(read(&shared), Ok(before));
    assert_eq!(local_file_read_only(&linked), Some(shared.clone()));

    let (second, stranded) = old_file("second");
    assert_eq!(local_file(&second), Ok(Some(shared)));
    assert!(stranded.exists(), "not merged into the shared file");
}

/// The session pointer names the review session the viewer has open, in
/// the shared git directory; a pointer dffy can't trust is an error.
#[test]
fn the_session_pointer_roundtrips() {
    let (_dir, d) = repository();
    let repo = Repo::discover(&d).expect("repo");
    assert_eq!(read_session(&repo), Ok(None));
    let pointer = Pointer {
        comments: PathBuf::from("/data/7/comments.json"),
        review: "7".to_owned(),
        branch: Some("feature/x".to_owned()),
        head: "e4f5a6b".to_owned(),
    };
    write_session(&repo, &pointer).expect("writes");
    assert_eq!(read_session(&repo), Ok(Some(pointer.clone())));
    let detached = Pointer {
        branch: None,
        ..pointer
    };
    write_session(&repo, &detached).expect("writes");
    assert_eq!(read_session(&repo), Ok(Some(detached)));
    let file = d.join(".git/dffy/session");
    for bad in [
        r#"{"version":1,"comments":"relative.json","review":"7","branch":null,"head":"e4f5a6b"}"#,
        r#"{"version":2,"comments":"/data/7/comments.json","review":"7","branch":null,"head":"e4f5a6b"}"#,
        r#"{"version":1,"review":"7","head":"e4f5a6b"}"#,
        "not json",
    ] {
        fs::write(&file, bad).expect("write");
        assert!(read_session(&repo).is_err(), "{bad}");
    }
}

/// A batch is applied whole or not at all, and the error names the
/// operation that failed.
#[test]
fn a_failed_batch_writes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = file_in(&dir);
    let input = format!(r#"[{LINE_COMMENT}, {{"op":"rm","id":"missing"}}]"#);
    let err = apply_json(&path, input.as_bytes(), None).expect_err("fails");
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
    let answer = apply_json(&path, input.as_bytes(), None).expect("applies");
    let value = json::parse(answer.as_bytes()).expect("valid JSON");
    let ids = value.get("ids").and_then(Value::as_array).expect("ids");
    let stored = read(&path).expect("reads");
    assert_eq!(
        ids,
        [Value::String(stored.comments[0].id.clone()), Value::Null]
    );
    assert!(path.with_file_name("comments.json.lock").exists());
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
            let result = apply_json(&path, format!("[{LINE_COMMENT}]").as_bytes(), None);
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
            thread::spawn(move || apply_json(&path, format!("[{LINE_COMMENT}]").as_bytes(), None))
        })
        .collect();
    for w in writers {
        w.join().expect("joins").expect("applies");
    }
    assert_eq!(read(&path).expect("reads").comments.len(), 9);
}

/// Threads go by path and line, each with its replies, and a reply whose
/// thread is gone gets a heading of its own rather than joining the thread
/// before it. A thread on another branch than the one listed says which,
/// and a comment says how its post went.
#[test]
fn listing_heads_every_thread() {
    let review = Review::parse(
        br#"{"version":1,"comments":[
{"id":"z1","path":"z.rs","text":"Orphan.","author":"","created":"t3","in_reply_to":"gone","resolved":false,"branch":"main"},
{"id":"b1","path":"a.rs","side":"old","start":{"old":4,"new":null},"end":{"old":5,"new":null},"text":"Two\nlines.","suggestion":"fixed","author":"","created":"t1","resolved":true,"anchor":{"lines":["x","y"]},"branch":"topic","post_error":"line 4 isn't in the diff"},
{"id":"a1","path":"a.rs","text":"File.","author":"ada","created":"t0","resolved":false},
{"id":"b2","path":"a.rs","text":"Reply.","author":"bob","created":"t2","in_reply_to":"b1","resolved":false,"ref":"r","branch":"topic","pending":true}
]}"#,
    )
    .expect("parses");
    let mut c = crate::render::Canvas::default();
    crate::print::comments(&threads(&review.comments), Some("main"), &[], &mut c);
    let mut out = Vec::new();
    c.write_to(&mut out, None).expect("writes");
    insta::assert_snapshot!(String::from_utf8(out).expect("utf-8"), @r"
    a.rs
      a1 ada, t0
        File.

    a.rs:4-5 (old side), branch topic, resolved
      b1 you, t1
        Two
        lines.
        suggestion:
          fixed
        not posted: line 4 isn't in the diff
      b2 bob, t2, posted, pending
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
        parse_ops(format!("[{LINE_COMMENT},{LINE_COMMENT}]").as_bytes(), None).expect("ops"),
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
    apply(
        &from,
        vec![Op::Set {
            target: Target::Id(moving.clone()),
            change: Box::new(Change {
                branch: Some(Some("topic".to_owned())),
                ..Change::default()
            }),
        }],
    )
    .expect("puts it on a branch");
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
    assert_eq!(
        (&root.branch, &got[2].branch),
        (&None, &None),
        "the review's now, on no branch"
    );
    assert_eq!(got[0].id, "pr1", "what was there stays");
}

/// `gone` says the host no longer has the comments with those refs. A
/// thread goes whole when every comment in it is gone; a gone reply goes
/// alone. A first comment, a general one too, whose thread still holds
/// replies that aren't gone, such as your drafts, stays as what they
/// answer: posted no longer, without the host's fields, and it can still
/// be resolved. Refs the file doesn't have are passed over.
#[test]
fn gone_removes_what_the_host_no_longer_has() {
    let mut c = Review::default();
    let comment = |reference: &str, extra: &str| {
        format!(
            r#"{{"op":"upsert","comment":{{"ref":"{reference}","path":"a.rs","text":"{reference}","author":"octocat","url":"https://x/{reference}","outdated":true,"pending":true,"host_resolved":true{extra}}}}}"#
        )
    };
    let reply = |reference: &str, root: &str| {
        format!(
            r#"{{"op":"upsert","comment":{{"ref":"{reference}","in_reply_to_ref":"{root}","text":"{reference}","author":"octocat"}}}}"#
        )
    };
    let input = [
        comment("x:whole", r#","x_thread":"T1""#),
        reply("x:whole-reply", "x:whole"),
        comment("x:kept", r#","x_thread":"T2""#),
        reply("x:kept-reply", "x:kept"),
        comment("x:drafted", r#","x_thread":"T3""#),
        r#"{"op":"upsert","comment":{"ref":"x:general","text":"general","author":"octocat"}}"#
            .to_owned(),
        reply("x:stays", "x:kept"),
    ]
    .join(",");
    ops(&mut c, &format!("[{input}]")).expect("applies");
    let drafted = c.comments[4].id.clone();
    let general = c.comments[5].id.clone();
    ops(
        &mut c,
        &format!(
            r#"[{{"op":"add","comment":{{"in_reply_to":"{drafted}","text":"My draft"}}}},{{"op":"add","comment":{{"in_reply_to":"{general}","text":"Also mine"}}}}]"#
        ),
    )
    .expect("drafts added");

    let ids = ops(
        &mut c,
        r#"[{"op":"gone","refs":["x:whole","x:whole-reply","x:kept-reply","x:drafted","x:general","x:never"]}]"#,
    )
    .expect("applies");
    assert_eq!(ids, [None]);
    let left: Vec<(&str, Option<&str>)> = c
        .comments
        .iter()
        .map(|c| (c.text.as_str(), c.reference.as_deref()))
        .collect();
    assert_eq!(
        left,
        [
            ("x:kept", Some("x:kept")),
            ("x:drafted", None),
            ("general", None),
            ("x:stays", Some("x:stays")),
            ("My draft", None),
            ("Also mine", None),
        ]
    );
    ops(
        &mut c,
        &format!(r#"[{{"op":"set","id":"{general}","fields":{{"resolved":true}}}}]"#),
    )
    .expect("a general comment kept without its ref is still one");
    assert!(
        ops(&mut c, r#"[{"op":"add","comment":{"text":"new"}}]"#).is_err(),
        "a new one still needs a ref"
    );
    let unposted = &c.comments[1];
    assert_eq!(
        (
            unposted.url.as_deref(),
            unposted.outdated,
            unposted.pending,
            unposted.host_resolved,
            unposted.extra.is_empty(),
            unposted.author.as_str(),
        ),
        (None, false, false, None, true, "octocat"),
        "posted no longer: the host's fields go, the words stay"
    );
    assert_eq!(
        c.comments[0].extra.len(),
        1,
        "a comment still there keeps its own"
    );
    assert!(
        ops(&mut c, r#"[{"op":"gone","refs":"x:kept"}]"#).is_err(),
        "refs is a list"
    );
}

/// A resolve or reopen not sent yet is yours: an import, which may have
/// read the thread before it, changes only what the host says of it. Once
/// the host has it, the host's word is the thread's again.
#[test]
fn an_unsent_resolve_outlasts_an_import() {
    let mut c = Review::default();
    let host = |resolved: bool| {
        format!(
            r#"[{{"op":"upsert","comment":{{"ref":"x:1","path":"a.rs","text":"Hm","author":"octocat","resolved":{resolved},"host_resolved":{resolved}}}}}]"#
        )
    };
    ops(&mut c, &host(false)).expect("imports");
    ops(
        &mut c,
        r#"[{"op":"set","ref":"x:1","fields":{"resolved":true}}]"#,
    )
    .expect("resolves");
    ops(&mut c, &host(false)).expect("imports again");
    assert_eq!(
        (c.comments[0].resolved, c.comments[0].host_resolved),
        (true, Some(false)),
        "still to send"
    );
    ops(&mut c, &host(true)).expect("the host has it");
    assert!(!c.comments[0].resolve_unsent());
    ops(&mut c, &host(false)).expect("reopened on the host");
    assert_eq!(
        (c.comments[0].resolved, c.comments[0].host_resolved),
        (false, Some(false)),
        "the host's again"
    );
}

/// A posted comment keeps its ref: another one for it is refused, as it
/// would leave the first on the host unknown here. Unposted, it takes one
/// again.
#[test]
fn a_posted_comment_keeps_its_ref() {
    let mut c = Review::default();
    let id = added(&mut c, &format!("[{LINE_COMMENT}]"));
    let set =
        |reference: &str| format!(r#"[{{"op":"set","id":"{id}","fields":{{"ref":{reference}}}}}]"#);
    ops(&mut c, &set(r#""x:1""#)).expect("posted");
    ops(&mut c, &set(r#""x:1""#)).expect("the same again");
    let refused = ops(&mut c, &set(r#""x:2""#)).expect_err("another");
    assert!(refused.contains("x:1"), "{refused}");
    ops(&mut c, &set("null")).expect("unposted");
    ops(&mut c, &set(r#""x:2""#)).expect("posted anew");
}

/// A thread of yours the host deleted, kept for a draft reply of yours in
/// it, says so as a draft the host refused: posting it again would start
/// a new thread, which the post dialog warns of first. Someone else's
/// stays theirs, which posting leaves.
#[test]
fn a_thread_of_yours_gone_from_the_host_says_so() {
    let mut c = Review::default();
    ops(
        &mut c,
        r#"[{"op":"upsert","comment":{"ref":"x:mine","path":"a.rs","text":"Mine","author":""}},{"op":"upsert","comment":{"ref":"x:theirs","path":"a.rs","text":"Theirs","author":"octocat"}}]"#,
    )
    .expect("imports");
    let (mine, theirs) = (c.comments[0].id.clone(), c.comments[1].id.clone());
    for root in [&mine, &theirs] {
        ops(
            &mut c,
            &format!(r#"[{{"op":"add","comment":{{"in_reply_to":"{root}","text":"Draft"}}}}]"#),
        )
        .expect("replies");
    }
    ops(&mut c, r#"[{"op":"gone","refs":["x:mine","x:theirs"]}]"#).expect("gone");
    let said: Vec<(&str, Option<&str>, Option<&str>)> = (c.comments.iter())
        .map(|c| {
            (
                c.text.as_str(),
                c.reference.as_deref(),
                c.post_error.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        said,
        [
            ("Mine", None, Some("deleted on the host")),
            ("Theirs", None, None),
            ("Draft", None, None),
            ("Draft", None, None),
        ]
    );
}

/// A move cut short, the other file holding the threads while the first
/// still does, moves nothing twice the next time: each copy carries the
/// id it was moved from.
#[test]
fn a_move_cut_short_isnt_repeated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (from, to) = (dir.path().join("local.json"), dir.path().join("pr.json"));
    let ops = parse_ops(format!("[{LINE_COMMENT}]").as_bytes(), None).expect("ops");
    let root = apply(&from, ops).expect("applies")[0].clone().expect("id");
    let reply = apply(
        &from,
        vec![Op::Add {
            comment: Box::new(Comment {
                text: "And this.".to_owned(),
                ..Comment::default()
            }),
            reply_to: Some(Target::Id(root.clone())),
        }],
    )
    .expect("replies")[0]
        .clone()
        .expect("id");
    // The first file can't be written back: its temporary file's name is
    // taken.
    let blocked = sibling(&from, ".tmp");
    fs::create_dir(&blocked).expect("mkdir");
    assert!(move_threads(&from, &to, std::slice::from_ref(&root)).is_err());
    assert_eq!(read(&from).expect("reads").comments.len(), 2, "still there");
    let copied = read(&to).expect("reads").comments;
    assert_eq!(
        (copied.iter())
            .map(|c| c.moved_from.as_deref())
            .collect::<Vec<_>>(),
        [Some(root.as_str()), Some(reply.as_str())]
    );
    fs::remove_dir(&blocked).expect("rmdir");
    assert_eq!(move_threads(&from, &to, std::slice::from_ref(&root)), Ok(2));
    assert_eq!(read(&from).expect("reads").comments.len(), 0);
    assert_eq!(read(&to).expect("reads").comments, copied, "nothing twice");
}

/// An import brings the summary when the file has none. A comment whose
/// id the file has for another comment comes in under a new id, and its
/// replies with it, wherever they are in the file; importing again still
/// adds nothing.
#[test]
fn an_import_keeps_other_comments_apart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (old, new) = (dir.path().join("old.json"), dir.path().join("7.json"));
    fs::write(
        &old,
        r#"{"version":1,"summary":"From before","comments":[
{"id":"r1","path":"a.rs","text":"Reply","author":"","created":"2026-11-17T11:00:00Z","in_reply_to":"a1"},
{"id":"a1","path":"a.rs","text":"Old","author":"","created":"2026-11-17T10:00:00Z"}
]}"#,
    )
    .expect("write");
    fs::write(
        &new,
        r#"{"version":1,"comments":[{"id":"a1","path":"b.rs","text":"Other","author":"","created":"2026-11-18T10:00:00Z"}]}"#,
    )
    .expect("write");
    assert_eq!(import(&old, &new).expect("imported"), 2);
    let got = read(&new).expect("read");
    assert_eq!(got.summary.as_deref(), Some("From before"));
    let (other, came, reply) = (&got.comments[0], &got.comments[1], &got.comments[2]);
    assert_eq!((other.id.as_str(), other.text.as_str()), ("a1", "Other"));
    assert_ne!(came.id, "a1", "a new id");
    assert_eq!(
        (came.text.as_str(), came.moved_from.as_deref()),
        ("Old", Some("a1"))
    );
    assert_eq!(
        (reply.text.as_str(), reply.in_reply_to.as_ref()),
        ("Reply", Some(&came.id))
    );
    assert_eq!(import(&old, &new).expect("again"), 0, "once");
    assert_eq!(read(&new).expect("read"), got);
}

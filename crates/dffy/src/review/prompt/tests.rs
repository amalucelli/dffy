use dffy_core::diff::{Options, hunks};
use dffy_core::git::{FileChange, Status};

use super::*;
use crate::review::Review;

/// `src/lib.rs` from `old` to `new`.
fn diff(old: &str, new: &str) -> FileDiff {
    diff_at("src/lib.rs", old, new)
}

fn diff_at(path: &str, old: &str, new: &str) -> FileDiff {
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    FileDiff {
        change: FileChange {
            path: path.into(),
            status: Status::Modified,
            old: None,
            new: None,
            staging: None,
        },
        hunks: hunks(&old, &new, &Options::default()),
        old,
        new,
        binary: false,
        too_large: false,
        note: None,
    }
}

/// Forty numbered lines, `changed` ones edited on the new side.
fn numbered(changed: impl Fn(usize) -> bool) -> String {
    (1..=40)
        .map(|i| {
            if changed(i) {
                format!("line {i} edited\n")
            } else {
                format!("line {i}\n")
            }
        })
        .collect()
}

/// ana's comment on new lines 5-6, and your reply with a suggestion
/// holding backticks.
const THREAD: &str = r#"{"version":1,"comments":[
{"id":"a1","path":"src/lib.rs","side":"new","start":{"old":null,"new":5},"end":{"old":null,"new":6},"text":"Why edit these?\n","author":"ana","created":"2026-11-17T10:00:00Z","resolved":false,"anchor":{"before":["line 3","line 4"],"lines":["line 5 edited","line 6"],"after":["line 7","line 8"]}},
{"id":"a2","path":"src/lib.rs","text":"Use `line 5` as it was.","suggestion":"line 5\nline 6 ```","author":"","created":"2026-11-17T11:00:00Z","in_reply_to":"a1","resolved":false}
]}"#;

fn review(json: &str) -> Review {
    Review::parse(json.as_bytes()).expect("parses")
}

fn on_new(lines: Range<usize>) -> At {
    At::Line {
        side: Side::New,
        lines,
    }
}

/// The thread's comments, authors and suggestion, where it is, its lines
/// marked among those around them, and the file's hunks near them.
#[test]
fn a_thread_prompt_holds_its_comments_lines_and_the_changes_near_them() {
    let review = review(THREAD);
    let thread: Vec<&Comment> = review.comments.iter().collect();
    let diff = diff(&numbered(|_| false), &numbered(|i| i == 5 || i == 25));
    let prompt = thread_within(
        false,
        &thread,
        &on_new(4..6),
        Some(&diff),
        Definitions::Parse,
        MAX_BYTES,
    );
    assert!(!prompt.trimmed);
    insta::assert_snapshot!(prompt.text);
}

/// Off the review's head, the prompt says so after where the thread is,
/// before anything a cut could take.
/// Off the review's head, the prompt warns the files on disk differ, right
/// after where the thread is, and no cut can take the warning.
#[test]
fn a_thread_prompt_off_the_reviews_head_says_the_files_differ() {
    let review = review(THREAD);
    let thread = [&review.comments[0]];
    let prompt = thread_within(true, &thread, &At::Lost, None, Definitions::Parse, 1);
    assert!(
        prompt.text.starts_with(
            "A review thread on src/lib.rs:5-6 (the new version of the file), as written.\n\
             Language: rust\n\nThe files on disk aren't this review's version (HEAD isn't its head); rely on the \
             hunks quoted.\n\n"
        ),
        "{}",
        prompt.text
    );
}

/// A selection says where it is, and has no thread to answer.
#[test]
fn a_selection_prompt_holds_its_lines_and_the_changes_near_them() {
    let diff = diff(&numbered(|_| false), &numbered(|i| i == 2));
    let prompt = selection_within(&diff, Side::Old, 1..2, Definitions::Parse, MAX_BYTES);
    insta::assert_snapshot!(prompt.text);
}

/// After where it is, a prompt names the file's language and, where its
/// language has definitions, those around all of its lines, outermost
/// first: parsed for the prompt, or only as the caller found them.
#[test]
fn a_prompt_names_the_language_and_the_definitions_around_its_lines() {
    let rust = |n: u8| {
        format!(
            "use std::fmt;\n\nmod tests {{\n    fn line_matches() {{\n        let a = {n};\n    \
             }}\n\n    fn other() {{}}\n}}\n"
        )
    };
    let diff = diff_at("src/lib.rs", &rust(1), &rust(2));
    let header = |text: &str| text.split("\n\n").next().unwrap_or_default().to_owned();
    assert_eq!(
        header(&selection_within(&diff, Side::New, 4..5, Definitions::Parse, MAX_BYTES).text),
        "Lines selected in a diff: src/lib.rs:5 (the new version of the file).\nLanguage: \
         rust\nIn: mod tests > fn line_matches"
    );
    let across = selection_within(&diff, Side::New, 4..8, Definitions::Parse, MAX_BYTES).text;
    assert!(across.contains("\nIn: mod tests\n\n"), "{across}");

    let review = review(THREAD);
    let thread = [&review.comments[0]];
    let text = thread_within(
        false,
        &thread,
        &on_new(4..5),
        Some(&diff),
        Definitions::Parse,
        MAX_BYTES,
    )
    .text;
    assert_eq!(
        header(&text),
        "A review thread on src/lib.rs:5 (the new version of the file).\nLanguage: rust\nIn: \
         mod tests > fn line_matches"
    );

    // The viewer's way: only definitions found already, so no parse.
    let found = Definitions::Found([None; 2]);
    assert_eq!(
        header(&selection_within(&diff, Side::New, 4..5, found, MAX_BYTES).text),
        "Lines selected in a diff: src/lib.rs:5 (the new version of the file).\nLanguage: rust"
    );

    let json = diff_at("config.json", "{\n  \"a\": 1\n}\n", "{\n  \"a\": 2\n}\n");
    assert_eq!(
        header(&selection_within(&json, Side::New, 1..2, Definitions::Parse, MAX_BYTES).text),
        "Lines selected in a diff: config.json:2 (the new version of the file).\nLanguage: json"
    );
}

/// A thread whose lines the diff doesn't have shows them as written,
/// numbered where they were, with the file's hunks in order.
#[test]
fn a_thread_whose_lines_are_gone_shows_them_as_written() {
    let review = review(THREAD);
    let thread = [&review.comments[0]];
    let diff = diff(&numbered(|_| false), &numbered(|i| i == 20));
    let lost = thread_within(
        false,
        &thread,
        &At::Lost,
        Some(&diff),
        Definitions::Parse,
        MAX_BYTES,
    );
    insta::assert_snapshot!(lost.text);
    let unseen = thread_within(
        false,
        &thread,
        &At::Lost,
        None,
        Definitions::Parse,
        MAX_BYTES,
    );
    assert!(
        unseen.text.starts_with(
            "A review thread on src/lib.rs:5-6 (the new version of the file), as written.\n"
        ),
        "{}",
        unseen.text
    );
}

/// A long thread on new lines 12-20 of a file changed at 3, 12-30 and 40:
/// first, two replies, one with a suggestion, then the last.
fn long_thread() -> (Review, FileDiff) {
    let review = review(
        r#"{"version":1,"comments":[
{"id":"b1","path":"src/lib.rs","side":"new","start":{"old":null,"new":12},"end":{"old":null,"new":20},"text":"First.","author":"ana","created":"t","resolved":false,"anchor":{"before":[],"lines":["line 12 edited"],"after":[]}},
{"id":"b2","path":"src/lib.rs","text":"A middle reply.","author":"bo","created":"t","in_reply_to":"b1","resolved":false},
{"id":"b3","path":"src/lib.rs","text":"Kept for its suggestion.","suggestion":"line 12","author":"","created":"t","in_reply_to":"b1","resolved":false},
{"id":"b4","path":"src/lib.rs","text":"Another middle reply.","author":"bo","created":"t","in_reply_to":"b1","resolved":false},
{"id":"b5","path":"src/lib.rs","text":"Last.","author":"ana","created":"t","in_reply_to":"b1","resolved":false}
]}"#,
    );
    let changed = |i: usize| i == 3 || (12..=30).contains(&i) || i == 40;
    (review, diff(&numbered(|_| false), &numbered(changed)))
}

/// Over the cap, the steps go in order: the farthest hunks, the anchored
/// hunk's lines past the window, the lines around down to three, the
/// replies before the last two but the one with a suggestion, then the
/// commented lines to their ends; each cut marked. What it's about, the
/// first and last two comments, the suggestion and the ask stay, past the
/// cap if they must.
#[test]
fn trimming_cuts_in_order_and_marks_each_cut() {
    let (review, diff) = long_thread();
    let thread: Vec<&Comment> = review.comments.iter().collect();
    let at = on_new(11..20);
    let within = |cap| thread_within(false, &thread, &at, Some(&diff), Definitions::Parse, cap);
    let full = within(MAX_BYTES).text;
    let has = |text: &str, n: usize| text.contains(&format!("+line {n} edited"));
    assert!(has(&full, 3) && has(&full, 40));

    let one = within(full.len() - 1);
    assert!(one.trimmed);
    assert!(has(&one.text, 3) && !has(&one.text, 40), "{}", one.text);
    assert!(
        one.text.contains("[1 other hunk of this file omitted]"),
        "{}",
        one.text
    );

    // At every cap between, a step is taken only once those before it are.
    let mut windowed_with_replies = false;
    for cap in (1..full.len()).step_by(40) {
        let text = within(cap).text;
        let replies_cut = text.contains("reply omitted");
        let lines_cut = text.contains("> 12  line 12 edited\n[…");
        let windowed = has(&text, 21) && !has(&text, 30);
        assert!(
            !windowed || !has(&text, 3) && !has(&text, 40),
            "{cap}: {text}"
        );
        assert!(
            !replies_cut || text.contains("  9  line 9\n  10"),
            "{cap}: {text}"
        );
        assert!(!lines_cut || replies_cut, "{cap}: {text}");
        windowed_with_replies |= windowed && !replies_cut;
    }
    assert!(
        windowed_with_replies,
        "a cap cuts the hunk and keeps the replies"
    );

    let least = within(1);
    assert!(least.trimmed);
    insta::assert_snapshot!(least.text);
}

/// A conversation with an agent cut to fit keeps its latest exchange: the
/// question, the answer before your follow-up, and the follow-up, which
/// the ask is to answer; older turns between go first.
#[test]
fn trimming_keeps_the_latest_exchange() {
    let review = review(
        r#"{"version":1,"comments":[
{"id":"c1","path":"src/lib.rs","side":"new","start":{"old":null,"new":5},"end":{"old":null,"new":5},"text":"Is this safe?","author":"","created":"t","resolved":false},
{"id":"c2","path":"src/lib.rs","text":"An old answer.","author":"claude","created":"t","in_reply_to":"c1","resolved":false},
{"id":"c3","path":"src/lib.rs","text":"An old follow-up.","author":"","created":"t","in_reply_to":"c1","resolved":false},
{"id":"c4","path":"src/lib.rs","text":"The latest answer.","author":"claude","created":"t","in_reply_to":"c1","resolved":false},
{"id":"c5","path":"src/lib.rs","text":"And then?","author":"","created":"t","in_reply_to":"c1","resolved":false}
]}"#,
    );
    let thread: Vec<&Comment> = review.comments.iter().collect();
    let diff = diff(&numbered(|_| false), &numbered(|i| i == 5));
    let least = thread_within(
        false,
        &thread,
        &on_new(4..5),
        Some(&diff),
        Definitions::Parse,
        1,
    )
    .text;
    for kept in ["Is this safe?", "The latest answer.", "And then?"] {
        assert!(least.contains(kept), "{kept}: {least}");
    }
    assert!(least.contains("[2 replies omitted]"), "{least}");
    assert!(
        least.ends_with("Answer the thread's last comment. To propose a change to the commented lines, end with the lines that replace them in a fenced block opened with ```suggestion.\n"),
        "{least}"
    );
}

/// A handed-off agent reads the thread where it's kept: the session's
/// with `--session`, another file named and quoted for a shell.
#[test]
fn a_handoff_points_to_where_the_thread_is_kept() {
    assert!(handoff("c1", Kept::Session).contains(
        "run dffy comments --session prompt c1 (or the thread_prompt MCP tool with session: true)"
    ));
    assert!(
        handoff("c1", Kept::File(Path::new("/tmp/my notes.json")))
            .contains("run dffy --comments '/tmp/my notes.json' comments prompt c1,")
    );
}

/// An agent handed every open thread reads them, and replies, in the file
/// they're kept in.
#[test]
fn an_open_threads_handoff_reads_and_replies_where_they_are_kept() {
    let file = Path::new("/tmp/my notes.json");
    assert_eq!(
        [Kept::Local, Kept::Session, Kept::File(file)].map(open_handoff),
        [
            "Address the open dffy review threads: run dffy comments prompt --open, change only \
             what each thread asks, reply in each thread with what you changed (dffy comments \
             apply --as <you>).",
            "Address the open dffy review threads: run dffy comments --session prompt --open, \
             change only what each thread asks, reply in each thread with what you changed (dffy \
             comments --session apply --as <you>).",
            "Address the open dffy review threads: run dffy --comments '/tmp/my notes.json' \
             comments prompt --open, change only what each thread asks, reply in each thread with \
             what you changed (dffy --comments '/tmp/my notes.json' comments apply --as <you>).",
        ]
    );
}

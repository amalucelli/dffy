use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, Instant};

use super::*;

const WAIT: Duration = Duration::from_secs(10);

const INIT: &str = r#"{"jsonrpc":"2.0","id":@ID@,"result":{"protocol":1,"command":"pr","noun":"pull request","nouns":"pull requests","capabilities":{"watch":true}}}"#;

const BASE: &str = "a1b2c3d000000000000000000000000000000000";
const HEAD: &str = "e4f5a6b000000000000000000000000000000000";

const SESSION: &str = r#"{"id":"7","title":"Spell out five","url":"https://github.com/acme/widgets/pull/7","base":"a1b2c3d000000000000000000000000000000000","head":"e4f5a6b000000000000000000000000000000000","last_reviewed_head":null,"checkout":"gh pr checkout 7"}"#;

/// The session [`SESSION`] names, its comments file in `reviews`.
fn seven(reviews: &Path) -> Session {
    Session {
        id: "7".to_owned(),
        label: None,
        title: Some("Spell out five".to_owned()),
        url: Some("https://github.com/acme/widgets/pull/7".to_owned()),
        base: BASE.to_owned(),
        head: HEAD.to_owned(),
        last_reviewed_head: None,
        state: None,
        labels: Vec::new(),
        comments: reviews.join("7.json"),
        checkout: Some("gh pr checkout 7".to_owned()),
    }
}

/// The test plugin in a directory of its own, answering `initialize` with
/// [`INIT`] until a test says otherwise.
struct Stub(tempfile::TempDir);

impl Stub {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dffy-review-test");
        std::fs::write(
            &path,
            include_str!("../../../tests/fixtures/stubs/review-plugin.sh"),
        )
        .expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let stub = Self(dir);
        stub.answer("initialize", &[INIT]);
        stub
    }

    /// Makes the message of `stem` answer `lines`.
    fn answer(&self, stem: &str, lines: &[&str]) {
        let out: String = lines.iter().flat_map(|l| [*l, "\n"]).collect();
        std::fs::write(self.0.path().join(format!("{stem}.out")), out).expect("write");
    }

    fn touch(&self, name: &str) -> PathBuf {
        let path = self.0.path().join(name);
        std::fs::write(&path, "").expect("write");
        path
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.0.path().join(name)).unwrap_or_default()
    }

    /// Where dffy keeps the plugin's reviews' comments.
    fn reviews(&self) -> PathBuf {
        self.0.path().join("reviews")
    }

    /// The stub as a plugin, in a process group of its own as the detached
    /// command puts it.
    fn start(&self) -> Plugin {
        let (path, dir) = (
            self.0.path().join("dffy-review-test"),
            self.0.path().to_owned(),
        );
        let command = move || {
            let mut command = Command::new(&path);
            command.current_dir(&dir).process_group(0);
            Ok(command)
        };
        let places = (Some(self.reviews()), None);
        Plugin::spawn("dffy-review-test".to_owned(), Box::new(command), places).expect("starts")
    }

    /// Waits until `file` has `lines` lines.
    fn wait_lines(&self, file: &str, lines: usize) {
        let start = Instant::now();
        while self.read(file).lines().count() < lines {
            assert!(start.elapsed() < WAIT, "{file}: {}", self.read(file));
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Everything `request` says, until its `Done`.
fn ask(plugin: &Plugin, request: &Request) -> Vec<Said> {
    let (tx, rx) = mpsc::channel();
    plugin.request(request, move |said| tx.send(said).is_ok());
    collect(&rx)
}

fn collect(rx: &mpsc::Receiver<Said>) -> Vec<Said> {
    let mut said = Vec::new();
    while !matches!(said.last(), Some(Said::Done(_))) {
        said.push(rx.recv_timeout(WAIT).expect("the request ends"));
    }
    said
}

fn parse(text: &str) -> Value {
    json::parse(text.as_bytes()).expect("JSON")
}

/// An `initialize` answer names a command and two nouns; without the rest,
/// a review reads as the noun and its id, and posting it is one event. A
/// command that isn't a word, or a noun that isn't one printable line, is
/// refused, and so is an answer of another protocol or none.
#[test]
fn an_initialize_answer_is_checked() {
    let gitlab = r#"{"command":"mr","noun":"merge request","nouns":"merge requests","protocol":1}"#;
    let init = Init::decode(&parse(gitlab)).expect("decodes");
    assert_eq!(
        init,
        Init {
            naming: Naming {
                command: "mr".to_owned(),
                noun: "merge request".to_owned(),
                nouns: "merge requests".to_owned(),
                short: "merge request".to_owned(),
                id_prefix: String::new(),
                events: default_events(),
                outdated_reply_warning: None,
            },
            watch: false,
        }
    );
    assert_eq!(init.naming.label("7"), "merge request 7");
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
        (gitlab.replace(",\"protocol\":1", ""), "names no protocol"),
    ] {
        let refused = Init::decode(&parse(&answer)).expect_err(&answer);
        assert!(refused.contains(why), "{answer}: {refused}");
    }
    let watching = gitlab.replace('}', r#","capabilities":{"watch":true}}"#);
    assert!(Init::decode(&parse(&watching)).expect("decodes").watch);
}

/// A plugin names a review's short form and id prefix, the events the post
/// dialog offers, in order, and a warning for replies to outdated threads.
/// An event's key can't be one of the dialog's own, nor repeated.
#[test]
fn an_initialize_answer_names_labels_and_events() {
    let full = r#"{"command":"mr","noun":"merge request","nouns":"merge requests","short":"MR","id_prefix":"!","outdated_reply_warning":"easy to miss","events":[{"id":"draft","key":"d","label":"keep as draft","done":"kept","submits":false},{"id":"approve","key":"a","label":"approve","done":"approved"}]}"#;
    let naming = Naming::decode(&parse(full)).expect("decodes");
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
            "other than j, k, l, s and v",
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
        let refused = Naming::decode(&parse(&answer)).expect_err(&answer);
        assert!(refused.contains(why), "{answer}: {refused}");
    }
}

/// A session may name labels to vote on beside the events, each a choice
/// of its own in the post dialog, as a Gerrit change's Code-Review and
/// Verified: a name, its values in order, each a whole number with words,
/// and your vote now, which the dialog starts on. They're the review's, so
/// `initialize` has none. Without labels there are none.
#[test]
fn a_session_names_labels_to_vote_on_with_your_vote() {
    let labels = r#""labels":[{"name":"Code-Review","values":[{"value":-2,"label":"Do not submit"},{"value":-1,"label":"I would prefer not"},{"value":0,"label":"No score"},{"value":1,"label":"Looks good to me, but someone else must approve"},{"value":2,"label":"Looks good to me, approved"}],"vote":1},{"name":"Verified","values":[{"value":-1,"label":"Fails"},{"value":0,"label":"No score"},{"value":1,"label":"Verified"}],"vote":0}]"#;
    let gerrit = SESSION.replace(
        r#""last_reviewed_head":null"#,
        &format!(r#""last_reviewed_head":null,{labels}"#),
    );
    let reviews = Some(Path::new("/r"));
    let session = session(&parse(&gerrit), reviews).expect("decodes");
    let value = |value: i32, label: &str| LabelValue {
        value,
        label: label.to_owned(),
    };
    assert_eq!(
        session.labels,
        [
            ReviewLabel {
                name: "Code-Review".to_owned(),
                values: vec![
                    value(-2, "Do not submit"),
                    value(-1, "I would prefer not"),
                    value(0, "No score"),
                    value(1, "Looks good to me, but someone else must approve"),
                    value(2, "Looks good to me, approved"),
                ],
                vote: 1,
            },
            ReviewLabel {
                name: "Verified".to_owned(),
                values: vec![
                    value(-1, "Fails"),
                    value(0, "No score"),
                    value(1, "Verified")
                ],
                vote: 0,
            },
        ]
    );
    let described = r#"{"command":"change","noun":"change","nouns":"changes"}"#;
    let with_labels = described.replace('}', &format!(",{labels}}}"));
    assert_eq!(
        Naming::decode(&parse(&with_labels)).expect("decodes"),
        Naming::decode(&parse(described)).expect("decodes"),
        "initialize's labels are no part of its naming"
    );
    for (answer, why) in [
        (
            gerrit.replace(r#""vote":0}]"#, r#""vote":3}]"#),
            "vote isn't one of its values",
        ),
        (gerrit.replace("Verified", "Code-Review"), "repeats a name"),
        (
            gerrit.replace(r#""value":-1,"label":"Fails""#, r#""value":0,"label":"Fails""#),
            "repeats a value",
        ),
        (
            gerrit.replace(r#""value":2,"#, r#""value":1.5,"#),
            "isn't a whole number",
        ),
        (
            gerrit.replace("Code-Review", "Code\\u001bReview"),
            "isn't 1 to 40 printable",
        ),
        (gerrit.replace(r#""values":[{"value":-1,"label":"Fails"},{"value":0,"label":"No score"},{"value":1,"label":"Verified"}]"#, r#""values":[]"#), "1 to 9 values"),
    ] {
        let refused = super::session(&parse(&answer), reviews).expect_err(&answer);
        assert!(refused.contains(why), "{answer}: {refused}");
    }
}

/// The contract with every review plugin: what each request about a
/// review carries, for the comments file and the local review's drafts in
/// tests/fixtures/review-protocol, is what requests.json there says, field
/// by field, and never the file itself. dffy-review-github's tests check
/// the requests they make against the same files.
#[test]
fn requests_carry_what_the_protocol_says_and_never_the_file() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/review-protocol");
    let expected = parse(&std::fs::read_to_string(dir.join("requests.json")).expect("read"));
    let expected = |method: &str| expected.get(method).expect(method).clone();
    let file = dir.join("review.json");
    let id = || "7".to_owned();
    let params = |request: &Request, file: &Path| {
        let (params, sent) = request.params(Some(file)).expect("params");
        assert!(!params.contains('\n'), "a message is a line");
        (parse(&params), sent)
    };
    assert_eq!(
        params(&Request::Prepare { id: id() }, &file).0,
        expected("prepare")
    );
    assert_eq!(
        params(&Request::Pull { id: id() }, &file).0,
        expected("prepare")
    );
    let dry_run = Request::DryRun {
        id: id(),
        drafts: Some((dir.join("local.json"), vec!["l1".to_owned()])),
    };
    assert_eq!(
        params(&dry_run, &file),
        (
            expected("dryRun"),
            Sent {
                pending: 1,
                resolves: 1
            }
        )
    );
    // A local draft not in view, as one on another branch, isn't planned.
    let unseen = Request::DryRun {
        id: id(),
        drafts: Some((dir.join("local.json"), Vec::new())),
    };
    let planned = params(&unseen, &file).0;
    let ids: Vec<&str> = (planned.get("drafts").and_then(Value::as_array))
        .unwrap_or_default()
        .iter()
        .filter_map(|d| d.get("id")?.as_str())
        .collect();
    // d4 replies in a thread kept local, which the host can't put it in.
    assert_eq!(ids, ["d1", "d2", "d3"]);
    let post = Request::Post {
        id: id(),
        event: "approve".to_owned(),
        labels: vec![("Code-Review".to_owned(), -1)],
        summary: Some("Ship it".to_owned()),
    };
    assert_eq!(params(&post, &file).0, expected("post"));

    let none = dir.join("none.json");
    assert_eq!(
        params(&Request::Prepare { id: id() }, &none).0,
        parse(r#"{"id":"7","known":[]}"#),
        "dffy has no file for the review yet"
    );
    assert_eq!(
        params(
            &Request::DryRun {
                id: id(),
                drafts: None
            },
            &none
        )
        .0,
        parse(r#"{"id":"7","drafts":[],"resolves":[]}"#)
    );
}

/// Each request's result reads by its method: `list` by id, with your
/// drafts in each review's comments file, `detect` as a session, a dry
/// run's drafts then their counts, with what waits on the host as the
/// request was made, a post's drafts left out then its counts. Fields dffy
/// doesn't know are passed over.
#[test]
fn results_read_by_method() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("9.json"),
        r#"{"version":1,"comments":[
{"id":"a","path":"a.rs","text":"Mine","author":"","created":"2026-11-17T10:00:00Z"},
{"id":"b","path":"a.rs","text":"Mine too","author":"","created":"2026-11-17T10:00:00Z"},
{"id":"c","path":"a.rs","text":"Posted","author":"","created":"2026-11-17T10:00:00Z","ref":"x:1"},
{"id":"d","path":"a.rs","text":"An agent's","author":"claude","created":"2026-11-17T10:00:00Z"},{"id":"e","path":"a.rs","text":"@claude why?","author":"","created":"2026-11-17T10:00:00Z","local_only":true},{"id":"f","path":"a.rs","text":"In its thread","author":"","created":"2026-11-17T10:00:00Z","in_reply_to":"e"}]}"#,
    )
    .expect("write");
    let sent = Sent {
        pending: 4,
        resolves: 1,
    };
    let said = |method: &str, result: &str| {
        answer(method, &parse(result), (Some(dir.path()), sent)).expect(result)
    };
    assert_eq!(
        said(
            "list",
            r#"{"reviews":[{"id":"9","title":"Add a cache","author":"hubot","reason":"review_requested","drafts":3},{"id":"I8f2c","label":"3","reason":"later","group":"Starred"},{"id":"1","group":"x\u001b[2Jy"}]}"#
        ),
        [Said::Listed(vec![
            Listed {
                id: "9".to_owned(),
                label: None,
                title: "Add a cache".to_owned(),
                author: "hubot".to_owned(),
                reason: Reason::ReviewRequested,
                group: None,
                drafts: 2,
            },
            Listed {
                id: "I8f2c".to_owned(),
                label: Some("3".to_owned()),
                title: String::new(),
                author: String::new(),
                reason: Reason::Other,
                group: Some("Starred".to_owned()),
                drafts: 0,
            },
            // A group that isn't one printable line is dropped.
            Listed {
                id: "1".to_owned(),
                label: None,
                title: String::new(),
                author: String::new(),
                reason: Reason::Other,
                group: None,
                drafts: 0,
            },
        ])]
    );
    assert_eq!(
        said("detect", &format!(r#"{{"review":{SESSION}}}"#)),
        [Said::Detected(Some(seven(dir.path())))]
    );
    assert_eq!(said("detect", r#"{"review":null}"#), [Said::Detected(None)]);
    let draft = |id: &str, plan| {
        Said::Draft(Draft {
            id: id.to_owned(),
            path: "src/lib.rs".to_owned(),
            plan,
        })
    };
    assert_eq!(
        said(
            "dryRun",
            r#"{"drafts":[{"id":"d1","path":"src/lib.rs","plan":"line","line":13,"side":"new"},{"id":"d2","path":"src/lib.rs","plan":"file"},{"id":"d3","path":"src/lib.rs","plan":"skip","reason":"file not in the PR"}],"line":9}"#
        ),
        [
            draft("d1", Plan::Line),
            draft("d2", Plan::File),
            draft("d3", Plan::Skip("file not in the PR".to_owned())),
            Said::Planned {
                line: 1,
                file: 1,
                skip: 1,
                pending: 4,
                resolves: 1,
            },
        ]
    );
    assert_eq!(
        said(
            "post",
            r#"{"unposted":[{"id":"d3","path":"src/lib.rs","plan":"skip","reason":"refused"}],"posted":11,"failed":1,"pending":4,"url":"https://x/pull/7#r","last_reviewed_head":"e4f5a6b000000000000000000000000000000000"}"#
        ),
        [
            draft("d3", Plan::Skip("refused".to_owned())),
            Said::Posted {
                posted: 11,
                failed: 1,
                pending: 4,
                url: Some("https://x/pull/7#r".to_owned()),
                last_reviewed_head: Some(HEAD.to_owned()),
                labels: None,
            }
        ]
    );
    assert_eq!(said("prepare", "{}"), []);
}

/// The head a post recorded as last reviewed is taken only as a whole
/// commit id, as a session's commits are.
#[test]
fn a_posts_last_reviewed_head_is_a_whole_commit_id() {
    let said = answer(
        "post",
        &parse(r#"{"posted":0,"failed":0,"pending":0,"url":null,"last_reviewed_head":"e4f5a6b"}"#),
        (None, Sent::default()),
    )
    .expect("read");
    assert!(
        matches!(
            &said[..],
            [Said::Posted {
                last_reviewed_head: None,
                ..
            }]
        ),
        "{said:?}"
    );
}

/// A post that changed your votes says the review's labels as they are now,
/// in a session's shape, so the next dialog starts on them; one that says
/// none leaves them as they were, and labels that break a session's rules
/// fail the answer.
#[test]
fn a_post_says_the_labels_its_votes_left() {
    let posted = |labels: &str| {
        answer(
            "post",
            &parse(&format!(
                r#"{{"posted":0,"failed":0,"pending":0,"url":null{labels}}}"#
            )),
            (None, Sent::default()),
        )
    };
    let voted = posted(
        r#","labels":[{"name":"Verified","values":[{"value":-1,"label":"Fails"},{"value":0,"label":"No score"},{"value":1,"label":"Verified"}],"vote":1}]"#,
    )
    .expect("read");
    let [
        Said::Posted {
            labels: Some(labels),
            ..
        },
    ] = &voted[..]
    else {
        panic!("{voted:?}");
    };
    assert_eq!(
        (labels.len(), labels[0].name.as_str(), labels[0].vote),
        (1, "Verified", 1)
    );
    assert!(
        matches!(
            &posted("").expect("read")[..],
            [Said::Posted { labels: None, .. }]
        ),
        "none said"
    );
    let broken = posted(r#","labels":[{"name":"Verified","values":[],"vote":0}]"#);
    assert!(broken.is_err(), "{broken:?}");
}

/// A session naming something other than whole commit ids, or a review id
/// that could pass for a flag, isn't taken; nor is one outside a
/// repository, which has nowhere to keep its comments.
#[test]
fn sessions_dffy_cant_trust_are_refused() {
    let reviews = Some(Path::new("/r"));
    let refused = |text: &str| session(&parse(text), reviews).expect_err(text);
    for base in ["--output=x", "a1b2c3d", &"a".repeat(41)] {
        let text = SESSION.replace(BASE, base);
        assert!(refused(&text).contains("base isn't a commit"), "{text}");
    }
    let sha256 = SESSION.replace(BASE, &"b".repeat(64));
    assert!(session(&parse(&sha256), reviews).is_ok(), "SHA-256's ids");
    for id in [r#""id":7"#, r#""id":"--output=x""#, r#""id":"""#] {
        let text = SESSION.replace(r#""id":"7""#, id);
        assert!(refused(&text).contains("no review id"), "{text}");
    }
    assert!(
        session(&parse(SESSION), None)
            .expect_err("nowhere")
            .contains("no repository")
    );
    for (state, read) in [
        ("open", Some(State::Open)),
        ("merged", Some(State::Merged)),
        ("locked", None),
    ] {
        let text = SESSION.replace("null", &format!(r#"null,"state":"{state}""#));
        assert_eq!(
            session(&parse(&text), reviews).expect("a session").state,
            read
        );
    }
}

/// A request's notifications come in order, its result last; requests
/// made before the plugin initialized wait for it. The plugin starts once,
/// in the directory it's given. Without a comments file for the review
/// yet, it knows of no comments, and what it writes is in dffy's file
/// before what follows it is said.
#[test]
fn a_request_says_its_notifications_then_its_result() {
    let stub = Stub::new();
    let gate = stub.touch("initialize.gate");
    stub.answer(
        "prepare",
        &[
            &format!(
                r#"{{"jsonrpc":"2.0","method":"session","params":{{"request":@ID@,"session":{SESSION}}}}}"#
            ),
            r#"{"jsonrpc":"2.0","method":"comments/apply","params":{"request":@ID@,"ops":[{"op":"upsert","comment":{"ref":"x:1","path":"a.rs","text":"Theirs","author":"octocat"}}]}}"#,
            r#"{"jsonrpc":"2.0","method":"progress","params":{"request":@ID@,"progress":"1/2"}}"#,
            r#"{"jsonrpc":"2.0","id":@ID@,"result":{}}"#,
        ],
    );
    let plugin = stub.start();
    let (tx, rx) = mpsc::channel();
    let read = stub.reviews().join("7.json");
    plugin.request(&Request::Prepare { id: "7".to_owned() }, move |said| {
        let texts: Vec<String> = (crate::review::read(&read).expect("read").comments)
            .into_iter()
            .map(|c| c.text)
            .collect();
        tx.send((said, texts)).is_ok()
    });
    std::fs::remove_file(gate).expect("open the gate");
    let said: Vec<(Said, Vec<String>)> = (0..3)
        .map(|_| rx.recv_timeout(WAIT).expect("said"))
        .collect();
    let texts = |t: &[&str]| t.iter().map(|&t| t.to_owned()).collect::<Vec<_>>();
    assert_eq!(
        said,
        [
            (Said::Session(seven(&stub.reviews())), texts(&[])),
            (Said::Progress("1/2".to_owned()), texts(&["Theirs"])),
            (Said::Done(Ok(())), texts(&["Theirs"])),
        ]
    );
    assert_eq!(stub.read("calls"), "prepare {\"id\":\"7\",\"known\":[]}\n");
    assert_eq!(stub.read("started").lines().count(), 1);
    plugin.close();
}

/// A failure's `data` says what was done before it, as a post stopped
/// midway; a method the plugin doesn't have is worded.
#[test]
fn an_error_says_what_was_done_and_an_unknown_method_is_worded() {
    let stub = Stub::new();
    stub.answer(
        "post",
        &[
            r#"{"jsonrpc":"2.0","id":@ID@,"error":{"code":1,"message":"post stopped","data":{"posted":2,"failed":1,"pending":0,"url":null}}}"#,
        ],
    );
    std::fs::create_dir_all(stub.reviews()).expect("mkdir");
    std::fs::write(
        stub.reviews().join("7.json"),
        r#"{"version":1,"comments":[
{"id":"d1","path":"a.rs","text":"Two\nlines","author":"","created":"2026-11-17T10:00:00Z"}
]}"#,
    )
    .expect("write");
    let plugin = stub.start();
    let post = Request::Post {
        id: "7".to_owned(),
        event: "comment".to_owned(),
        labels: Vec::new(),
        summary: Some("Ship \"it\"\n".to_owned()),
    };
    assert_eq!(
        ask(&plugin, &post),
        [
            Said::Posted {
                posted: 2,
                failed: 1,
                pending: 0,
                url: None,
                last_reviewed_head: None,
                labels: None,
            },
            Said::Done(Err("post stopped".to_owned()))
        ]
    );
    // One line, the file's drafts in it.
    assert_eq!(
        stub.read("calls"),
        "post {\"id\":\"7\",\"event\":\"comment\",\"summary\":\"Ship \\\"it\\\"\\n\",\"drafts\":[{\"id\":\"d1\",\"path\":\"a.rs\",\"text\":\"Two\\nlines\"}],\"resolves\":[]}\n"
    );
    assert_eq!(
        ask(&plugin, &Request::List),
        [Said::Done(Err(
            "dffy-review-test doesn't know list; it may be older than this dffy".to_owned()
        ))]
    );
    plugin.close();
}

/// Operations that can't be written ask the plugin to stop, and the
/// request ends saying why; what it sends before it stops is still
/// written, as a post's refs must be. A review without a comments file
/// yet has no drafts to send.
#[test]
fn what_cant_be_written_stops_the_request_and_what_follows_is_written() {
    let stub = Stub::new();
    stub.answer(
        "post",
        &[
            r#"{"jsonrpc":"2.0","method":"comments/apply","params":{"request":@ID@,"ops":[{"op":"set","id":"gone","fields":{"ref":"x:1"}}]}}"#,
            r#"{"jsonrpc":"2.0","method":"comments/apply","params":{"request":@ID@,"ops":[{"op":"upsert","comment":{"ref":"x:2","path":"a.rs","text":"Posted","author":""}}]}}"#,
            r#"{"jsonrpc":"2.0","id":@ID@,"result":{"posted":1,"failed":0,"pending":0,"url":null}}"#,
        ],
    );
    let plugin = stub.start();
    let post = Request::Post {
        id: "7".to_owned(),
        event: "comment".to_owned(),
        labels: Vec::new(),
        summary: None,
    };
    let said = ask(&plugin, &post);
    assert!(
        matches!(said.last(), Some(Said::Done(Err(e))) if e.starts_with("dffy-review-test's comments not written: ")),
        "{said:?}"
    );
    let file = crate::review::read(&stub.reviews().join("7.json")).expect("read");
    let refs: Vec<_> = file
        .comments
        .iter()
        .map(|c| c.reference.as_deref())
        .collect();
    assert_eq!(refs, [Some("x:2")]);
    stub.wait_lines("calls", 2);
    let calls = stub.read("calls");
    assert!(
        calls.starts_with(
            "post {\"id\":\"7\",\"event\":\"comment\",\"drafts\":[],\"resolves\":[]}\n"
        ) && calls.ends_with("$/cancelRequest {\"id\":2}\n"),
        "{calls}"
    );
    plugin.close();
}

/// A request dffy ends before the plugin answers, as here on a session it
/// can't take, asks the plugin to stop; what the plugin sends for it until
/// it answers is still written, and nothing after the answer is.
#[test]
fn a_request_ended_here_is_written_until_the_plugin_answers() {
    let stub = Stub::new();
    let bad = SESSION.replace(BASE, "a1b2c3d");
    stub.answer(
        "prepare",
        &[
            &format!(
                r#"{{"jsonrpc":"2.0","method":"session","params":{{"request":@ID@,"session":{bad}}}}}"#
            ),
            r#"{"jsonrpc":"2.0","method":"comments/apply","params":{"request":@ID@,"ops":[{"op":"upsert","comment":{"ref":"x:1","path":"a.rs","text":"Before","author":"octocat"}}]}}"#,
            r#"{"jsonrpc":"2.0","id":@ID@,"result":{}}"#,
            r#"{"jsonrpc":"2.0","method":"comments/apply","params":{"request":@ID@,"ops":[{"op":"upsert","comment":{"ref":"x:2","path":"a.rs","text":"After","author":"octocat"}}]}}"#,
        ],
    );
    stub.answer(
        "detect",
        &[r#"{"jsonrpc":"2.0","id":@ID@,"result":{"review":null}}"#],
    );
    let plugin = stub.start();
    let said = ask(&plugin, &Request::Prepare { id: "7".to_owned() });
    assert!(
        matches!(&said[..], [Said::Done(Err(e))] if e.contains("base isn't a commit")),
        "{said:?}"
    );
    // Messages are read in order: once `detect` is answered, every line
    // of the `prepare` was.
    ask(&plugin, &Request::Detect);
    let file = crate::review::read(&stub.reviews().join("7.json")).expect("read");
    let texts: Vec<&str> = file.comments.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(texts, ["Before"]);
    let calls = stub.read("calls");
    assert!(
        calls.contains("$/cancelRequest {\"id\":2}\n"),
        "the plugin is asked to stop: {calls}"
    );
    plugin.close();
}

/// A plugin that crashes partway through a post keeps what it sent: the
/// refs written so far stay, and the next post doesn't send those drafts
/// again.
#[test]
fn a_post_cut_short_by_a_crash_keeps_its_refs() {
    let stub = Stub::new();
    std::fs::create_dir_all(stub.reviews()).expect("mkdir");
    std::fs::write(
        stub.reviews().join("7.json"),
        r#"{"version":1,"comments":[
{"id":"d1","path":"a.rs","text":"First","author":"","created":"2026-11-17T10:00:00Z"},
{"id":"d2","path":"a.rs","text":"Second","author":"","created":"2026-11-17T10:00:00Z"}
]}"#,
    )
    .expect("write");
    stub.answer(
        "post",
        &[r#"{"jsonrpc":"2.0","method":"comments/apply","params":{"request":@ID@,"ops":[{"op":"set","id":"d1","fields":{"ref":"x:1"}}]}}"#],
    );
    let die = stub.touch("post.die");
    let plugin = stub.start();
    let post = Request::Post {
        id: "7".to_owned(),
        event: "comment".to_owned(),
        labels: Vec::new(),
        summary: None,
    };
    let said = ask(&plugin, &post);
    assert!(
        matches!(&said[..], [Said::Done(Err(e))] if e.contains("exited with status 3")),
        "{said:?}"
    );
    let file = crate::review::read(&stub.reviews().join("7.json")).expect("read");
    assert_eq!(file.comments[0].reference.as_deref(), Some("x:1"));
    std::fs::remove_file(die).expect("no more crashes");
    stub.answer(
        "post",
        &[r#"{"jsonrpc":"2.0","id":@ID@,"result":{"posted":1,"failed":0,"pending":0,"url":null}}"#],
    );
    stub.wait_lines("started", 2);
    ask(&plugin, &post);
    let calls = stub.read("calls");
    let second = calls.lines().filter(|l| l.starts_with("post ")).nth(1);
    assert_eq!(
        second,
        Some(
            r#"post {"id":"7","event":"comment","drafts":[{"id":"d2","path":"a.rs","text":"Second"}],"resolves":[]}"#
        ),
        "{calls}"
    );
    plugin.close();
}

/// Once a post with an event that submits ends well, nothing in the review
/// is visible only to you any more, so dffy clears every `pending` itself,
/// before the post says it's done. An event that leaves the review open
/// keeps them, and so does a post that failed.
#[test]
fn a_submitted_review_has_nothing_pending() {
    let stub = Stub::new();
    stub.answer(
        "initialize",
        &[&INIT.replace(
            r#""capabilities""#,
            r#""events":[{"id":"pending","key":"p","label":"keep pending","done":"kept","submits":false},{"id":"comment","key":"c","label":"comment","done":"commented"}],"capabilities""#,
        )],
    );
    stub.answer(
        "post",
        &[r#"{"jsonrpc":"2.0","id":@ID@,"result":{"posted":0,"failed":0,"pending":1,"url":null}}"#],
    );
    std::fs::create_dir_all(stub.reviews()).expect("mkdir");
    let file = stub.reviews().join("7.json");
    std::fs::write(
        &file,
        r#"{"version":1,"comments":[
{"id":"c1","path":"a.rs","text":"Posted, pending","author":"","created":"2026-11-17T10:00:00Z","ref":"x:1","pending":true}
]}"#,
    )
    .expect("write");
    let plugin = stub.start();
    // Each thing said, with whether the comment was pending as it came.
    let post = |event: &str| {
        let (tx, rx) = mpsc::channel();
        let file = file.clone();
        let request = Request::Post {
            id: "7".to_owned(),
            event: event.to_owned(),
            labels: Vec::new(),
            summary: None,
        };
        plugin.request(&request, move |said| {
            let pending = crate::review::read(&file).expect("read").comments[0].pending;
            tx.send((said, pending)).is_ok()
        });
        (0..2)
            .map(|_| rx.recv_timeout(WAIT).expect("said"))
            .collect::<Vec<_>>()
    };
    assert_eq!(post("pending")[1], (Said::Done(Ok(())), true), "still open");
    assert_eq!(
        post("comment")[1],
        (Said::Done(Ok(())), false),
        "cleared before it's done"
    );
    plugin.close();
}

/// A plugin of another protocol is refused, its requests fail saying so,
/// and it isn't started again.
#[test]
fn a_refused_initialize_fails_every_request() {
    let stub = Stub::new();
    stub.answer(
        "initialize",
        &[&INIT.replace(r#""protocol":1"#, r#""protocol":2"#)],
    );
    let plugin = stub.start();
    let said = ask(&plugin, &Request::Detect);
    assert!(
        matches!(&said[..], [Said::Done(Err(e))] if e.contains("speaks review protocol 2")),
        "{said:?}"
    );
    let (tx, rx) = mpsc::channel();
    plugin.on_ready(move |naming| {
        tx.send(naming).ok();
    });
    assert!(rx.recv_timeout(WAIT).expect("said").is_err());
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(stub.read("started").lines().count(), 1, "not again");
}

/// Cancelling sends `$/cancelRequest` for it, and the request ends as the
/// plugin answers.
#[test]
fn a_cancelled_request_ends_with_the_plugins_answer() {
    let stub = Stub::new();
    stub.touch("prepare.gate");
    stub.answer("prepare", &[r#"{"jsonrpc":"2.0","id":@ID@,"result":{}}"#]);
    stub.answer(
        "_cancelRequest",
        &[r#"{"jsonrpc":"2.0","id":@ID@,"error":{"code":-32800,"message":"cancelled"}}"#],
    );
    let plugin = stub.start();
    let (tx, rx) = mpsc::channel();
    let run = plugin.request(&Request::Prepare { id: "7".to_owned() }, move |said| {
        tx.send(said).is_ok()
    });
    stub.wait_lines("calls", 1);
    run.cancel();
    assert_eq!(collect(&rx), [Said::Done(Err("cancelled".to_owned()))]);
    let calls = stub.read("calls");
    let id = calls
        .lines()
        .nth(1)
        .and_then(|l| l.strip_prefix("$/cancelRequest {\"id\":"))
        .expect("the cancel");
    assert_eq!(id, "2}", "{calls}");
    plugin.close();
}

/// A plugin that exits fails the requests it left, then starts again, told
/// again which review dffy shows; one that doesn't watch isn't told.
#[test]
fn a_crashed_plugin_fails_its_requests_and_starts_again() {
    let stub = Stub::new();
    let crash = stub.touch("list.crash");
    let plugin = stub.start();
    plugin.watch(Some("7"));
    let said = ask(&plugin, &Request::List);
    assert!(
        matches!(&said[..], [Said::Done(Err(e))] if e.contains("dffy-review-test exited with status 3")),
        "{said:?}"
    );
    std::fs::remove_file(crash).expect("no more crashes");
    stub.wait_lines("started", 2);
    stub.answer(
        "list",
        &[r#"{"jsonrpc":"2.0","id":@ID@,"result":{"reviews":[]}}"#],
    );
    assert_eq!(
        ask(&plugin, &Request::List),
        [Said::Listed(Vec::new()), Said::Done(Ok(()))]
    );
    let watched = stub
        .read("calls")
        .lines()
        .filter(|l| *l == "watch {\"id\":\"7\"}")
        .count();
    assert_eq!(watched, 2, "{}", stub.read("calls"));
    plugin.close();

    let quiet = Stub::new();
    quiet.answer(
        "initialize",
        &[&INIT.replace(r#","capabilities":{"watch":true}"#, "")],
    );
    let plugin = quiet.start();
    plugin.watch(Some("7"));
    plugin.idle(true);
    assert_eq!(
        ask(&plugin, &Request::Detect).last(),
        Some(&Said::Done(Err(
            "dffy-review-test doesn't know detect; it may be older than this dffy".to_owned()
        )))
    );
    assert_eq!(quiet.read("calls"), "detect {}\n");
    plugin.close();
}

/// What a watching plugin pushes reaches dffy: the review's head, new
/// comments, words for the status bar.
#[test]
fn a_watching_plugin_pushes_what_changed() {
    let stub = Stub::new();
    stub.answer(
        "watch",
        &[
            r#"{"jsonrpc":"2.0","method":"head/moved","params":{"id":"7","head":"cccc333ccccccccccccccccccccccccccccccccc"}}"#,
            r#"{"jsonrpc":"2.0","method":"head/moved","params":{"id":"7","head":"--x"}}"#,
            r#"{"jsonrpc":"2.0","method":"comments/changed","params":{"id":"7"}}"#,
            r#"{"jsonrpc":"2.0","method":"log","params":{"message":"checking failed"}}"#,
        ],
    );
    stub.answer("idle_changed", &[]);
    let plugin = stub.start();
    plugin.watch(Some("7"));
    // Pushed before anything takes them, as a `prepare` dffy started
    // before its viewer may: they wait.
    stub.wait_lines("calls", 1);
    std::thread::sleep(Duration::from_millis(200));
    let (tx, rx) = mpsc::channel();
    plugin.on_push(move |push| tx.send(push).is_ok());
    let pushed: Vec<Push> = (0..3)
        .map(|_| rx.recv_timeout(WAIT).expect("a push"))
        .collect();
    assert_eq!(
        pushed,
        [
            Push::HeadMoved {
                id: "7".to_owned(),
                head: "cccc333ccccccccccccccccccccccccccccccccc".to_owned()
            },
            Push::CommentsChanged { id: "7".to_owned() },
            Push::Log("checking failed".to_owned()),
        ]
    );
    plugin.idle(true);
    plugin.idle(true);
    plugin.idle(false);
    stub.wait_lines("calls", 3);
    assert_eq!(
        stub.read("calls"),
        "watch {\"id\":\"7\"}\nidle/changed {\"idle\":true}\nidle/changed {\"idle\":false}\n"
    );
    plugin.close();
}

/// Closing the plugin while a crashed process is being started again,
/// past the pause, kills the new one instead of keeping it.
#[test]
fn closing_during_a_restart_keeps_it_closed() {
    let stub = Stub::new();
    stub.touch("initialize.crash");
    let (path, dir) = (
        stub.0.path().join("dffy-review-test"),
        stub.0.path().to_owned(),
    );
    let (at_start, started) = mpsc::channel();
    let (go_on, wait) = mpsc::channel::<()>();
    let wait = Mutex::new(wait);
    let starts = std::sync::atomic::AtomicUsize::new(0);
    let command = move || {
        if starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            at_start.send(()).ok();
            wait.lock().expect("lock").recv_timeout(WAIT).ok();
        }
        let mut command = Command::new(&path);
        command.current_dir(&dir).process_group(0);
        Ok(command)
    };
    let plugin = Plugin::spawn(
        "dffy-review-test".to_owned(),
        Box::new(command),
        (Some(stub.reviews()), None),
    )
    .expect("starts");
    started.recv_timeout(WAIT).expect("started again");
    plugin.close();
    go_on.send(()).expect("go on");
    // Long enough for the new process to be started, and for a request
    // queued on it to end with its crash, were it kept.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        ask(&plugin, &Request::List).last(),
        Some(&Said::Done(Err("dffy-review-test was stopped".to_owned())))
    );
}

/// Closing the plugin kills what it started too, and its requests end.
#[test]
fn closing_takes_what_the_plugin_started_along() {
    let stub = Stub::new();
    stub.touch("prepare.hold");
    stub.answer(
        "prepare",
        &[r#"{"jsonrpc":"2.0","method":"progress","params":{"request":@ID@,"progress":"0/1"}}"#],
    );
    let plugin = stub.start();
    let (tx, rx) = mpsc::channel();
    plugin.request(&Request::Prepare { id: "7".to_owned() }, move |said| {
        tx.send(said).is_ok()
    });
    assert_eq!(
        rx.recv_timeout(WAIT).expect("progress"),
        Said::Progress("0/1".to_owned())
    );
    stub.wait_lines("pid", 1);
    let started = Instant::now();
    plugin.close();
    assert!(
        matches!(rx.recv_timeout(WAIT), Ok(Said::Done(Err(_)))),
        "the request ends"
    );
    assert!(started.elapsed() < WAIT, "it didn't wait for the sleep");
    let sleep: i32 = stub.read("pid").trim().parse().expect("a pid");
    let sleep = Pid::from_raw(sleep).expect("a pid");
    let gone = Instant::now();
    while rustix::process::test_kill_process(sleep).is_ok() {
        assert!(gone.elapsed() < WAIT, "the sleep outlived the kill");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(stub.read("started").lines().count(), 1, "not again");
}

/// A post holds its review's post lock from reading its drafts until it
/// ends, so another viewer's post of that review is refused before it
/// reaches its plugin; it lets go before its end is told, so the other can
/// post as soon as it hears.
#[test]
fn a_review_is_posted_by_one_viewer_at_a_time() {
    let stub = Stub::new();
    let gate = stub.touch("post.gate");
    stub.answer(
        "post",
        &[r#"{"jsonrpc":"2.0","id":@ID@,"result":{"posted":0,"failed":0,"pending":0}}"#],
    );
    let (one, two) = (stub.start(), stub.start());
    let post = Request::Post {
        id: "7".to_owned(),
        event: "post".to_owned(),
        labels: Vec::new(),
        summary: None,
    };
    let (tx, rx) = mpsc::channel();
    let (free_tx, free) = mpsc::channel();
    let file = crate::review::review_file(&stub.reviews(), "7");
    one.request(&post, move |said| {
        if matches!(said, Said::Done(_)) {
            // A few tries: a process another test forks meanwhile holds
            // the lock's file a moment, until its exec closes it. Held
            // until this returns, as it was, every try fails.
            let free = (0..50).any(|_| {
                let free = crate::review::lock_post(&file).is_ok();
                if !free {
                    std::thread::sleep(Duration::from_millis(2));
                }
                free
            });
            free_tx.send(free).ok();
        }
        tx.send(said).is_ok()
    });
    stub.wait_lines("calls", 1);
    let refused = ask(&two, &post);
    assert!(
        matches!(refused.last(), Some(Said::Done(Err(e))) if e.contains("another dffy is posting")),
        "{refused:?}"
    );
    assert_eq!(stub.read("calls").lines().count(), 1, "never sent");
    std::fs::remove_file(gate).expect("open the gate");
    assert_eq!(collect(&rx).last(), Some(&Said::Done(Ok(()))));
    assert_eq!(free.recv_timeout(WAIT), Ok(true), "let go before Done");
    assert_eq!(ask(&two, &post).last(), Some(&Said::Done(Ok(()))));
    one.close();
    two.close();
}

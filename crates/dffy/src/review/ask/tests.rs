use super::super::model::reload::Check;
use super::*;
use crate::review;

const FILE: &str = r#"{"version":1,"comments":[
{"id":"c1","path":"a.rs","side":"new","start":{"old":null,"new":2},"end":{"old":null,"new":2},"text":"Can this overflow?","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":[],"lines":["x + y"],"after":[]}}
]}"#;

/// A review session's comments in `dir`, a thread `c1` in them: your
/// comments there are drafts, and an agent's are never posted.
fn comments(dir: &tempfile::TempDir) -> (Comments, std::path::PathBuf) {
    let path = dir.path().join("7.json");
    std::fs::write(&path, FILE).expect("write");
    let mut comments = Comments::new(path.clone(), None, true, None);
    comments.refresh(Check::Stat).expect("reads");
    (comments, path)
}

fn codex() -> Agent {
    Agent {
        name: "codex".to_owned(),
        default: true,
        mark: Some("read-only sandbox".to_owned()),
    }
}

fn claude() -> Agent {
    Agent {
        name: "claude".to_owned(),
        default: false,
        mark: None,
    }
}

/// An ask goes to the plugin, says what the agent is doing while it runs,
/// and its answer lands in the thread as the agent's reply, its
/// suggestion and its mark with it: someone else's, which posting leaves
/// out.
#[test]
fn an_answer_is_the_agents_reply_in_its_thread() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut comments, path) = comments(&dir);
    let mut asking = Asking::default();
    let asked = asking.ask("c1".to_owned(), "the prompt".to_owned(), (None, None));
    assert_eq!(
        asked,
        [
            Effect::Agent {
                id: 1,
                request: Request::Ask {
                    agent: None,
                    prompt: "the prompt".to_owned()
                }
            },
            Effect::Redraw
        ]
    );
    assert_eq!(asking.status_besides(None).as_deref(), Some("asking…"));
    assert_eq!(asking.status_besides(Some("c1")), None);
    asking.said((1, Said::Asking(codex())), Some(&mut comments));
    asking.said(
        (1, Said::Progress("thinking…".to_owned())),
        Some(&mut comments),
    );
    assert_eq!(
        asking.status_besides(None).as_deref(),
        Some("asking codex… thinking…")
    );
    assert!(asking.asking("c1"));
    let answer = Answer {
        text: "Yes, past u32::MAX.\n".to_owned(),
        suggestion: Some("x.checked_add(y)?".to_owned()),
    };
    let said = asking.said((1, Said::Answered(Ok(answer))), Some(&mut comments));
    assert_eq!(
        said,
        [
            Effect::Changed(vec!["a.rs".into()]),
            Effect::Say("codex answered".to_owned()),
            Effect::Redraw
        ]
    );
    assert_eq!(asking.status_besides(None), None);
    let file = review::read(&path).expect("reads");
    let reply = file.comments.last().expect("a reply");
    assert_eq!(
        (
            reply.in_reply_to.as_deref(),
            reply.author.as_str(),
            reply.text.as_str(),
            reply.suggestion.as_deref(),
            reply.label.as_deref()
        ),
        (
            Some("c1"),
            "codex",
            "Yes, past u32::MAX.",
            Some("x.checked_add(y)?"),
            Some("read-only sandbox")
        )
    );
    let ids: Vec<&str> = comments.drafts().map(|(id, ..)| id).collect();
    assert_eq!(ids, ["c1"], "the agent's reply isn't a draft");
}

/// One ask a thread at a time; a stopped ask is told to the plugin, and
/// what it says after is ignored, its answer included.
#[test]
fn a_stopped_ask_writes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut comments, path) = comments(&dir);
    let mut asking = Asking::default();
    asking.ask("c1".to_owned(), "p".to_owned(), (None, None));
    asking.said((1, Said::Asking(claude())), Some(&mut comments));
    assert_eq!(
        asking.ask(
            "c1".to_owned(),
            "p".to_owned(),
            (Some("codex".to_owned()), None)
        ),
        [Effect::Say(
            "already asking claude about this thread".to_owned()
        )]
    );
    assert_eq!(asking.stop(Some("c2")), [], "another thread's");
    assert_eq!(
        asking.stop(Some("c1")),
        [
            Effect::StopAgents(vec![1]),
            Effect::Say("stopped asking claude".to_owned()),
            Effect::Redraw
        ]
    );
    let late = Answer {
        text: "Too late.".to_owned(),
        suggestion: None,
    };
    assert_eq!(
        asking.said((1, Said::Answered(Ok(late))), Some(&mut comments)),
        []
    );
    assert_eq!(review::read(&path).expect("reads").comments.len(), 1);
    assert!(!asking.busy());
}

/// A failed ask says the plugin's words, and so does an agent it doesn't
/// have. Without the plugin installed an ask says so, but a mention, which
/// may name a person, says nothing.
#[test]
fn failures_say_why() {
    let mut asking = Asking::default();
    asking.ask("c1".to_owned(), "p".to_owned(), (None, None));
    let logged_out = "claude isn't logged in; run `claude auth login`";
    assert_eq!(
        asking.said((1, Said::Answered(Err(logged_out.to_owned()))), None),
        [Effect::Fail(logged_out.to_owned()), Effect::Redraw]
    );
    asking.ask("c1".to_owned(), "p".to_owned(), (None, None));
    assert_eq!(
        asking.said((2, Said::NotInstalled), None),
        [
            Effect::Fail("dffy-agent isn't on PATH (see docs/plugins.md)".to_owned()),
            Effect::Redraw
        ]
    );
    asking.ask(
        "c1".to_owned(),
        "p".to_owned(),
        (Some("octocat".to_owned()), Some("c1".to_owned())),
    );
    assert_eq!(asking.said((3, Said::NotInstalled), None), [Effect::Redraw]);
    assert!(!asking.busy());
}

/// A mention is written kept local, as on the host it would reach whoever
/// has the name there. An agent that takes the ask keeps it so; when none
/// does, no plugin or no agent of that name, it's a draft again.
#[test]
fn a_mention_no_agent_takes_is_a_draft_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut comments, path) = comments(&dir);
    let mention = |text: &str| Op::Add {
        comment: Box::new(Comment {
            text: text.to_owned(),
            local_only: true,
            ..Comment::default()
        }),
        reply_to: Some(Target::Id("c1".to_owned())),
    };
    let (_, ids) = comments
        .apply(vec![mention("@claude why?"), mention("@octocat why?")])
        .expect("applies");
    let [Some(asked), Some(person)] = &ids[..] else {
        panic!("{ids:?}");
    };
    let mut asking = Asking::default();
    let ask = |asking: &mut Asking, id: &str| {
        asking.ask("c1".to_owned(), "p".to_owned(), (None, Some(id.to_owned())));
    };
    ask(&mut asking, asked);
    asking.said((1, Said::Asking(claude())), Some(&mut comments));
    let answer = Answer {
        text: "Because.".to_owned(),
        suggestion: None,
    };
    asking.said((1, Said::Answered(Ok(answer))), Some(&mut comments));
    ask(&mut asking, person);
    let no_agent = "no agent named octocat; claude".to_owned();
    assert_eq!(
        asking.said(
            (2, Said::Answered(Err(no_agent.clone()))),
            Some(&mut comments)
        ),
        [
            Effect::Changed(vec!["a.rs".into()]),
            Effect::Fail(no_agent),
            Effect::Redraw
        ]
    );
    let local: Vec<(String, bool)> = (review::read(&path).expect("reads").comments.into_iter())
        .map(|c| (c.text, c.local_only))
        .collect();
    assert_eq!(
        local[1..],
        [
            ("@claude why?".to_owned(), true),
            ("@octocat why?".to_owned(), false),
            ("Because.".to_owned(), false),
        ]
    );
    let ids: Vec<&str> = comments.drafts().map(|(id, ..)| id).collect();
    assert_eq!(ids, ["c1", person.as_str()]);
}

/// A comment's first word names an agent with `@`: any name until the
/// plugin listed its agents, then only theirs.
#[test]
fn a_mention_names_an_agent() {
    let mut asking = Asking::default();
    let mentioned = |asking: &Asking, text: &str| asking.mentioned(text);
    assert_eq!(
        mentioned(&asking, "@claude why?").as_deref(),
        Some("claude")
    );
    assert_eq!(
        mentioned(&asking, " @Octocat, look").as_deref(),
        Some("octocat")
    );
    assert_eq!(mentioned(&asking, "ask @claude"), None);
    assert_eq!(mentioned(&asking, "@ claude"), None);
    assert_eq!(asking.default_agent(), None);
    asking.listed(Ok(vec![claude(), codex()]));
    assert_eq!(mentioned(&asking, "@octocat, look"), None);
    assert_eq!(
        mentioned(&asking, "@codex: is it safe?").as_deref(),
        Some("codex")
    );
    assert_eq!(asking.default_agent(), Some("codex"));
}

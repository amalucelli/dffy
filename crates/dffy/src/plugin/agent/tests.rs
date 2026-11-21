use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use super::*;

const WAIT: Duration = Duration::from_secs(10);

/// Where the protocol's messages put the programs they name, as `@BIN@`.
const BIN: &str = "/opt/agents/bin";

/// The protocol's messages, which dffy-agent's tests hold the plugin to,
/// their programs under [`BIN`].
fn messages() -> Value {
    let text = include_str!("../../../tests/fixtures/agent-protocol/messages.json");
    json::parse(text.replace("@BIN@", BIN).as_bytes()).expect("JSON")
}

fn message(name: &str, part: &str) -> Value {
    let messages = messages();
    messages
        .get(name)
        .and_then(|m| m.get(part))
        .unwrap_or_else(|| panic!("{name}.{part}"))
        .clone()
}

fn encoded(value: &Value) -> String {
    let mut out = String::new();
    json::write(&mut out, value);
    out
}

/// A response to request `@ID@` holding `value` under `key`.
fn response(key: &str, value: &Value) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":@ID@,"{key}":{}}}"#,
        encoded(value)
    )
}

/// The test plugin as `dffy-agent` in a directory of its own, which lists
/// the agents of the protocol's `initialize` answer.
struct Stub(tempfile::TempDir);

impl Stub {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(PROGRAM);
        std::fs::write(
            &path,
            include_str!("../../../tests/fixtures/stubs/review-plugin.sh"),
        )
        .expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let stub = Self(dir);
        stub.answer(
            "initialize",
            &[&response("result", &message("initialize", "result"))],
        );
        stub
    }

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

    /// The stub as the plugin, with `config` as the config's `agent`, its
    /// asks reading `/`.
    fn start(&self, config: Option<&str>) -> Plugin {
        let path = self.0.path().join(PROGRAM);
        let command = move || {
            let mut command = Command::new(&path);
            command.process_group(0);
            Ok(command)
        };
        let state = State {
            config: config.map(str::to_owned),
            cwd: "/".to_owned(),
        };
        Plugin::spawn(Box::new(command), state).expect("starts")
    }
}

/// Everything `request` says, until it ends.
fn ask(plugin: &Plugin, request: &Request) -> (Run, Vec<Said>) {
    let (tx, rx) = mpsc::channel();
    let run = plugin.request(request, move |said| {
        tx.send(said).ok();
    });
    let mut said = Vec::new();
    while !matches!(said.last(), Some(Said::Answered(_) | Said::Checks(_))) {
        said.push(rx.recv_timeout(WAIT).expect("the request ends"));
    }
    (run, said)
}

fn prompt(name: &str) -> String {
    let params = message(name, "params");
    params
        .get("prompt")
        .and_then(Value::as_str)
        .expect("a prompt")
        .to_owned()
}

/// dffy's `initialize` carries the config's `agent` unread, and its `ask`
/// the agent picked, the prompt and the working tree, as the protocol's
/// messages have them; what the plugin says back reads as they mean.
#[test]
fn an_ask_carries_and_reads_what_the_protocol_says() {
    let stub = Stub::new();
    let progress: Vec<String> = (message("ask", "progress").as_array().expect("a list"))
        .iter()
        .map(|p| {
            format!(
                r#"{{"jsonrpc":"2.0","method":"progress","params":{{"request":@ID@,"progress":{}}}}}"#,
                encoded(p)
            )
        })
        .collect();
    let mut lines: Vec<&str> = progress.iter().map(String::as_str).collect();
    let result = response("result", &message("ask", "result"));
    lines.push(&result);
    stub.answer("ask", &lines);
    let plugin = stub.start(Some("codex"));
    let request = Request::Ask {
        agent: Some("claude".to_owned()),
        prompt: prompt("ask"),
    };
    let (_, said) = ask(&plugin, &request);
    let claude = Agent {
        name: "claude".to_owned(),
        default: false,
        mark: None,
    };
    let mut expected = vec![Said::Asking(claude)];
    expected.extend(["starting…", "thinking…", "writing…"].map(|p| Said::Progress(p.to_owned())));
    expected.push(Said::Answered(Ok(Answer {
        text: "Yes: `x + y` overflows past `u32::MAX`.".to_owned(),
        suggestion: Some("let total = x\n    .checked_add(y)?;".to_owned()),
    })));
    assert_eq!(said, expected);
    plugin.close();

    let mut init = message("initialize", "params");
    if let Value::Object(fields) = &mut init
        && let Some((_, Value::Object(client))) = fields.iter_mut().find(|(k, _)| k == "client")
    {
        client.retain(|(k, _)| k != "version");
        client.push((
            "version".to_owned(),
            Value::String(env!("CARGO_PKG_VERSION").to_owned()),
        ));
    }
    let sent = stub.read("initialized");
    assert_eq!(json::parse(sent.trim().as_bytes()), Ok(init), "{sent}");
    let calls = stub.read("calls");
    let asked = calls.lines().next().and_then(|l| l.strip_prefix("ask "));
    let asked = json::parse(asked.expect(&calls).as_bytes());
    assert_eq!(asked, Ok(message("ask", "params")));
}

/// Without a config value, `initialize` has no `agent`, and an ask without
/// an agent goes to the one the plugin calls the default.
#[test]
fn an_ask_without_an_agent_goes_to_the_default() {
    let stub = Stub::new();
    stub.answer(
        "ask",
        &[r#"{"jsonrpc":"2.0","id":@ID@,"result":{"text":"Fine."}}"#],
    );
    let plugin = stub.start(None);
    let request = Request::Ask {
        agent: None,
        prompt: "Is it?\n".to_owned(),
    };
    let (_, said) = ask(&plugin, &request);
    let codex = Agent {
        name: "codex".to_owned(),
        default: true,
        mark: Some("read-only sandbox".to_owned()),
    };
    assert_eq!(
        said,
        [
            Said::Asking(codex),
            Said::Answered(Ok(Answer {
                text: "Fine.".to_owned(),
                suggestion: None
            }))
        ]
    );
    plugin.close();
    assert!(!stub.read("initialized").contains("agent"));
    assert!(stub.read("calls").starts_with("ask {\"agent\":\"codex\","));
}

/// A failed ask ends with the plugin's own words, a login hint included;
/// an agent it doesn't list is refused before anything is sent, in the
/// words the plugin would use.
#[test]
fn a_failed_ask_says_the_plugins_message() {
    let stub = Stub::new();
    stub.answer(
        "ask",
        &[&response("error", &message("notLoggedIn", "error"))],
    );
    let plugin = stub.start(None);
    let ended = |agent: &str| {
        let request = Request::Ask {
            agent: Some(agent.to_owned()),
            prompt: prompt("notLoggedIn"),
        };
        ask(&plugin, &request).1.pop()
    };
    assert_eq!(
        ended("claude"),
        Some(Said::Answered(Err(
            "claude isn't logged in; run `claude auth login`".to_owned()
        )))
    );
    let unknown = message("unknownAgent", "error");
    let words = unknown.get("message").and_then(Value::as_str);
    assert_eq!(
        ended("gemini"),
        Some(Said::Answered(Err(words.expect("a message").to_owned())))
    );
    plugin.close();
    assert_eq!(
        stub.read("calls").matches("ask ").count(),
        1,
        "gemini isn't asked"
    );
}

/// A cancelled ask is asked to stop with `$/cancelRequest`, and ends as
/// the plugin answers it.
#[test]
fn a_cancelled_ask_asks_the_plugin_to_stop() {
    let stub = Stub::new();
    let gate = stub.touch("ask.gate");
    stub.answer(
        "ask",
        &[r#"{"jsonrpc":"2.0","id":@ID@,"result":{"text":"Too late."}}"#],
    );
    stub.answer(
        "_cancelRequest",
        &[&response("error", &message("cancelled", "error"))],
    );
    let plugin = stub.start(None);
    let (tx, rx) = mpsc::channel();
    let request = Request::Ask {
        agent: None,
        prompt: prompt("cancelled"),
    };
    let run = plugin.request(&request, move |said| {
        tx.send(said).ok();
    });
    assert!(matches!(rx.recv_timeout(WAIT), Ok(Said::Asking(_))));
    run.cancel();
    assert_eq!(
        rx.recv_timeout(WAIT),
        Ok(Said::Answered(Err("cancelled".to_owned())))
    );
    assert!(stub.read("calls").contains("$/cancelRequest {\"id\":"));
    std::fs::remove_file(gate).expect("open the gate");
    plugin.close();
}

#[test]
fn doctor_reads_the_plugins_checks() {
    let stub = Stub::new();
    stub.answer(
        "doctor",
        &[&response("result", &message("doctor", "result"))],
    );
    let plugin = stub.start(None);
    let (_, said) = ask(&plugin, &Request::Doctor);
    plugin.close();
    let Some(Said::Checks(Ok(checks))) = said.last() else {
        panic!("{said:?}");
    };
    assert_eq!(checks.len(), 7);
    assert_eq!(
        checks[0],
        Check {
            name: "claude".to_owned(),
            ok: true,
            detail: "2.1.329 (Claude Code)".to_owned(),
            brief: Some("2.1.329".to_owned()),
            info: false,
        }
    );
    assert!(checks[6].ok && checks[6].info, "{:?}", checks[6]);
}

/// The agents' names are what a mention names, and their marks one
/// printable line; a handoff names its program by an absolute path, and
/// takes its prompt in its arguments or on the clipboard; another protocol
/// is refused.
#[test]
fn an_initialize_answer_is_checked() {
    let parse = |text: &str| json::parse(text.as_bytes()).expect("JSON");
    let good = r#"{"protocol":1,"agents":[{"name":"claude","default":true,"mark":null}],"handoffs":[{"name":"amp","program":"/bin/amp","args":["{prompt}"]}]}"#;
    assert_eq!(
        Init::decode(&parse(good)).map(|i| (i.agents.len(), i.handoffs.len())),
        Ok((1, 1))
    );
    for (answer, why) in [
        (
            good.replace("\"/bin/amp\"", "\"amp\""),
            "amp's program isn't an absolute path",
        ),
        (
            good.replace("[\"{prompt}\"]", "[]"),
            "amp's handoff takes no prompt",
        ),
        (
            good.replace("[\"{prompt}\"]", "[1]"),
            "amp's args aren't text",
        ),
        (
            good.replace("[\"{prompt}\"]", "[],\"prompt\":\"stdin\""),
            "amp's prompt is stdin, and dffy knows only clipboard",
        ),
        (good.replace("\"amp\"", "\"A m p\""), "a handoff's name"),
        (good.replace("claude", "Claude Code"), "an agent's name"),
        (
            good.replace("null", "\"read\\u001b[2Jonly\""),
            "mark isn't 1 to 60 printable",
        ),
        (good.replace(":1,", ":2,"), "speaks agent protocol 2"),
        (
            good.replace(",\"agents\":[", ",\"none\":["),
            "lists no agents",
        ),
    ] {
        let refused = Init::decode(&parse(&answer)).expect_err(&answer);
        assert!(refused.contains(why), "{answer}: {refused}");
    }
}

/// The handoffs `initialize` describes, read as the protocol's messages
/// have them: an agent on its prompt as an argument, or one whose prompt
/// goes on the clipboard, which no ask goes to. Waiting for them doesn't
/// take the place of waiting for the agents.
#[test]
fn the_handoffs_read_as_the_protocol_says() {
    let stub = Stub::new();
    let plugin = stub.start(None);
    let (tx, rx) = mpsc::channel();
    let agents = tx.clone();
    plugin.on_ready(move |listed| {
        agents.send(listed.map(|l| l.len().to_string())).ok();
    });
    for config in [None, Some("amp")] {
        let tx = tx.clone();
        plugin.on_handoff(config.map(str::to_owned), move |handoff| {
            tx.send(handoff.map(|h| format!("{h:?}"))).ok();
        });
    }
    let said: Vec<_> = (0..3)
        .map(|_| rx.recv_timeout(WAIT).expect("said"))
        .collect();
    plugin.close();
    assert_eq!(
        said,
        [
            Ok("2".to_owned()),
            Ok(format!(
                "{:?}",
                Handoff {
                    name: "claude".to_owned(),
                    program: PathBuf::from(format!("{BIN}/claude")),
                    args: vec!["{prompt}".to_owned()],
                    clipboard: false,
                }
            )),
            Ok(format!(
                "{:?}",
                Handoff {
                    name: "amp".to_owned(),
                    program: PathBuf::from(format!("{BIN}/amp")),
                    args: Vec::new(),
                    clipboard: true,
                }
            )),
        ]
    );
}

/// A thread goes to the agent the config names, else to the first the
/// plugin lists; a command of yours in the config takes no thread, so the
/// first takes it. A name the plugin doesn't list is refused.
#[test]
fn a_thread_goes_to_the_agent_picked_from_the_handoffs() {
    let listing = |names: &[&str]| {
        let handoffs: Vec<String> = (names.iter())
            .map(|name| {
                format!(r#"{{"name":"{name}","program":"/bin/{name}","args":["{{prompt}}"]}}"#)
            })
            .collect();
        let answer = format!(
            r#"{{"protocol":1,"agents":[],"handoffs":[{}]}}"#,
            handoffs.join(",")
        );
        Init::decode(&json::parse(answer.as_bytes()).expect("JSON")).expect("decodes")
    };
    let pick = |init: &Init, config| init.handoff(config).map(|h| h.name.clone());
    let all = listing(&["claude", "codex", "pi", "amp"]);
    assert_eq!(pick(&all, None), Ok("claude".to_owned()));
    assert_eq!(pick(&all, Some("amp")), Ok("amp".to_owned()));
    assert_eq!(pick(&all, Some("my-agent --fast")), Ok("claude".to_owned()));
    assert_eq!(pick(&listing(&["pi", "amp"]), None), Ok("pi".to_owned()));
    assert_eq!(
        pick(&listing(&["codex"]), Some("claude")),
        Err("no agent named claude to hand the thread to; codex".to_owned())
    );
    assert_eq!(
        pick(&listing(&[]), None),
        Err("dffy-agent lists no agent to hand the thread to; dffy doctor says why".to_owned())
    );
}

/// A handoff's command is its program and arguments, the prompt in place
/// of `{prompt}`; one whose prompt goes on the clipboard has none of it.
#[test]
fn a_handoffs_command_carries_the_prompt_where_it_says() {
    let handoff = |args: &[&str], clipboard| Handoff {
        name: "agent".to_owned(),
        program: PathBuf::from("/bin/agent"),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        clipboard,
    };
    assert_eq!(
        handoff(&["--fast", "{prompt}"], false).command("Address it"),
        ["/bin/agent", "--fast", "Address it"].map(OsString::from)
    );
    assert_eq!(
        handoff(&[], true).command("Address it"),
        ["/bin/agent"].map(OsString::from)
    );
}

//! The built binary end to end, as dffy runs it: a session over stdin and
//! stdout, JSON-RPC a line, with stub `claude`, `codex`, `pi` and custom agent
//! programs on a PATH of their own that log their arguments, directory and
//! stdin, and print canned answers. The environment is emptied but for
//! PATH, HOME and the stubs' directory, so the user's agents never run.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(30);

const CLAUDE_ARGS: [&str; 14] = [
    "-p",
    "--verbose",
    "--output-format",
    "stream-json",
    "--tools",
    "Read,Grep,Glob",
    "--disallowedTools",
    "mcp__*",
    "--strict-mcp-config",
    "--setting-sources",
    "user",
    "--no-session-persistence",
    "--permission-mode",
    "dontAsk",
];

/// The agent protocol's messages, shared with dffy's own tests.
fn messages() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/dffy/tests/fixtures/agent-protocol/messages.json");
    serde_json::from_slice(&fs::read(path).expect("the protocol's messages")).expect("JSON")
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        for sub in [
            "bin",
            "stub/claude",
            "stub/codex",
            "stub/pi",
            "stub/my-agent",
            "work",
        ] {
            fs::create_dir_all(dir.path().join(sub)).expect("dir");
        }
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// The worktree an ask runs in, as the plugin sees it.
    fn work(&self) -> PathBuf {
        self.path("work").canonicalize().expect("canonical")
    }

    fn put(&self, rel: &str, contents: &str) {
        fs::write(self.path(rel), contents).expect("written");
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path(rel)).unwrap_or_else(|_| panic!("{rel} wasn't written"))
    }

    /// Puts `script` on PATH as `name`.
    fn install(&self, name: &str, script: &str) -> &Self {
        let path = self.path(&format!("bin/{name}"));
        fs::write(&path, script).expect("stub");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        self
    }

    fn claude(&self) -> &Self {
        self.install("claude", include_str!("fixtures/stubs/claude.sh"))
    }

    fn codex(&self) -> &Self {
        self.install("codex", include_str!("fixtures/stubs/codex.sh"))
    }

    fn pi(&self) -> &Self {
        self.install("pi", include_str!("fixtures/stubs/pi.sh"))
    }

    fn start(&self) -> Session {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dffy-agent"))
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.path("bin").display()),
            )
            .env("HOME", self.dir.path())
            .env("STUB", self.path("stub"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("runs");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let message = serde_json::from_str(&line)
                    .unwrap_or_else(|_| panic!("a message isn't JSON: {line}"));
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        Session {
            child,
            stdin,
            said: rx,
            next_id: 0,
        }
    }

    /// A session, initialized with `agent` as dffy's config has it.
    fn session(&self, agent: Option<&str>) -> (Session, Value) {
        let mut session = self.start();
        let mut params = json!({"protocol": 1, "client": {"name": "dffy", "version": "0.3.0"}});
        if let Some(agent) = agent {
            params["agent"] = json!(agent);
        }
        let (_, init) = session.call("initialize", &params);
        (session, init.expect("initialized"))
    }

    fn ask(&self, agent: &str, prompt: &str) -> (Vec<Value>, Result<Value, Value>) {
        let (mut session, _) = self.session(None);
        let answer = session.call(
            "ask",
            &json!({"agent": agent, "prompt": prompt, "cwd": self.work()}),
        );
        session.close();
        answer
    }
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    said: mpsc::Receiver<Value>,
    next_id: u64,
}

impl Session {
    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("sent");
    }

    fn next(&self) -> Value {
        self.said.recv_timeout(WAIT).expect("an answer in time")
    }

    /// Sends `method`, returning its id.
    fn request(&mut self, method: &str, params: &Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// The notifications before request `id`'s answer, then the answer: its
    /// result, or its error.
    fn answer(&self, id: u64) -> (Vec<Value>, Result<Value, Value>) {
        let mut notes = Vec::new();
        loop {
            let message = self.next();
            if message["id"] == id {
                return (
                    notes,
                    match message.get("error") {
                        Some(error) => Err(error.clone()),
                        None => Ok(message["result"].clone()),
                    },
                );
            }
            notes.push(message);
        }
    }

    fn call(&mut self, method: &str, params: &Value) -> (Vec<Value>, Result<Value, Value>) {
        let id = self.request(method, params);
        self.answer(id)
    }

    fn notify(&mut self, method: &str, params: &Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn close(mut self) {
        let (_, done) = self.call("shutdown", &Value::Null);
        assert_eq!(done, Ok(Value::Null));
        self.notify("exit", &Value::Null);
        assert!(self.child.wait().expect("exits").success());
    }
}

/// The progress texts among `notes`, each for request `id`.
fn progress(notes: &[Value], id: u64) -> Vec<String> {
    notes
        .iter()
        .filter(|n| n["method"] == "progress")
        .map(|n| {
            assert_eq!(n["params"]["request"], id, "{n}");
            n["params"]["progress"]
                .as_str()
                .expect("progress text")
                .to_owned()
        })
        .collect()
}

fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .expect("kill runs")
        .success()
}

#[test]
fn initialize_lists_only_the_agents_found() {
    let f = Fixture::new();
    f.claude();
    let (session, init) = f.session(None);
    session.close();
    assert_eq!(
        init,
        json!({"protocol": 1, "capabilities": {},
            "agents": [{"name": "claude", "default": true, "mark": null}]})
    );
}

#[test]
fn initialize_refuses_another_protocol() {
    let f = Fixture::new();
    let mut session = f.start();
    let (_, init) = session.call("initialize", &json!({"protocol": 2}));
    session.close();
    let error = init.expect_err("refused");
    assert_eq!(error["code"], -32602);
    assert_eq!(
        error["message"],
        "dffy-agent speaks agent protocol 1, and dffy asked for 2; \
         install the dffy-agent that matches your dffy"
    );
}

/// The protocol's messages, as dffy's tests hold dffy to them.
#[test]
fn the_plugin_says_what_the_protocol_fixtures_say() {
    let m = messages();
    let f = Fixture::new();
    f.claude().codex();
    f.put(
        "stub/claude/answer.jsonl",
        include_str!("fixtures/claude-suggestion.jsonl"),
    );
    let mut session = f.start();
    let (_, init) = session.call("initialize", &m["initialize"]["params"]);
    assert_eq!(init, Ok(m["initialize"]["result"].clone()));

    let id = session.request("ask", &m["ask"]["params"]);
    let (notes, answer) = session.answer(id);
    assert_eq!(answer, Ok(m["ask"]["result"].clone()));
    assert_eq!(json!(progress(&notes, id)), m["ask"]["progress"]);

    let (_, doctor) = session.call("doctor", &m["doctor"]["params"]);
    assert_eq!(doctor, Ok(m["doctor"]["result"].clone()));

    let (_, unknown) = session.call("ask", &m["unknownAgent"]["params"]);
    assert_eq!(unknown, Err(m["unknownAgent"]["error"].clone()));

    f.put(
        "stub/claude/answer.jsonl",
        include_str!("fixtures/claude-error.jsonl"),
    );
    f.put("stub/claude/logged-out", "");
    let (_, logged_out) = session.call("ask", &m["notLoggedIn"]["params"]);
    assert_eq!(logged_out, Err(m["notLoggedIn"]["error"].clone()));

    fs::remove_file(f.path("stub/claude/logged-out")).expect("logged in");
    f.put("stub/claude/sleep", "");
    let id = session.request("ask", &m["cancelled"]["params"]);
    let start = Instant::now();
    while !f.path("stub/claude/pids").exists() {
        assert!(start.elapsed() < WAIT, "claude didn't start");
        std::thread::sleep(Duration::from_millis(5));
    }
    session.notify("$/cancelRequest", &json!({"id": id}));
    let (_, cancelled) = session.answer(id);
    assert_eq!(cancelled, Err(m["cancelled"]["error"].clone()));
    session.close();
}

#[test]
fn claude_answers_through_its_read_only_recipe_with_the_prompt_on_stdin() {
    let f = Fixture::new();
    f.claude();
    f.put(
        "stub/claude/answer.jsonl",
        include_str!("fixtures/claude-answer.jsonl"),
    );
    let (notes, answer) = f.ask("claude", "Can this overflow?\n");
    assert_eq!(
        answer,
        Ok(json!({"text": "It can overflow when both are near `u32::MAX`."}))
    );
    assert_eq!(progress(&notes, 2), ["starting…", "thinking…", "writing…"]);
    assert_eq!(lines(&f.read("stub/claude/args")), CLAUDE_ARGS);
    assert_eq!(f.read("stub/claude/stdin"), "Can this overflow?\n");
    assert_eq!(
        f.read("stub/claude/cwd").trim_end(),
        f.work().to_str().expect("utf-8")
    );
}

#[test]
fn a_suggestion_block_becomes_the_suggestion_and_leaves_the_text() {
    let f = Fixture::new();
    f.claude();
    f.put(
        "stub/claude/answer.jsonl",
        include_str!("fixtures/claude-suggestion.jsonl"),
    );
    let (_, answer) = f.ask("claude", "Can this overflow?");
    assert_eq!(
        answer,
        Ok(json!({"text": "Yes: `x + y` overflows past `u32::MAX`.",
            "suggestion": "let total = x\n    .checked_add(y)?;"}))
    );
}

#[test]
fn codex_answers_with_its_last_message_through_its_read_only_recipe() {
    let f = Fixture::new();
    f.codex();
    f.put(
        "stub/codex/answer.jsonl",
        include_str!("fixtures/codex-answer.jsonl"),
    );
    let (notes, answer) = f.ask("codex", "Can this overflow?\n");
    assert_eq!(
        answer,
        Ok(json!({"text": "It can overflow; `checked_add` says so."}))
    );
    assert_eq!(progress(&notes, 2), ["starting…", "thinking…", "writing…"]);
    let work = f.work();
    let work = work.to_str().expect("utf-8");
    assert_eq!(
        lines(&f.read("stub/codex/args")),
        [
            "exec",
            "--json",
            "--sandbox",
            "read-only",
            "--ephemeral",
            "-c",
            "web_search=\"disabled\"",
            "-C",
            work,
        ]
    );
    assert_eq!(f.read("stub/codex/stdin"), "Can this overflow?\n");
    assert_eq!(f.read("stub/codex/cwd").trim_end(), work);
}

#[test]
fn pi_answers_with_its_last_message_through_its_read_only_recipe() {
    let f = Fixture::new();
    f.pi();
    f.put(
        "stub/pi/answer.jsonl",
        include_str!("fixtures/pi-answer.jsonl"),
    );
    let (notes, answer) = f.ask("pi", "Can this overflow?\n");
    assert_eq!(
        answer,
        Ok(json!({"text": "It can overflow when both are near `u32::MAX`."}))
    );
    assert_eq!(
        progress(&notes, 2),
        [
            "starting…",
            "thinking…",
            "reading…",
            "searching…",
            "writing…"
        ]
    );
    assert_eq!(
        lines(&f.read("stub/pi/args")),
        [
            "--mode",
            "json",
            "--no-session",
            "--tools",
            "read,grep,find,ls",
            "--no-mcp",
            "--no-approve",
            "--offline",
        ]
    );
    assert_eq!(f.read("stub/pi/stdin"), "Can this overflow?\n");
    assert_eq!(
        f.read("stub/pi/cwd").trim_end(),
        f.work().to_str().expect("utf-8")
    );
}

/// Its text blocks join, and a suggestion in them is taken out.
#[test]
fn pis_text_blocks_make_the_answer_and_its_suggestion() {
    let f = Fixture::new();
    f.pi();
    f.put(
        "stub/pi/answer.jsonl",
        include_str!("fixtures/pi-suggestion.jsonl"),
    );
    assert_eq!(
        f.ask("pi", "Can this overflow?").1,
        Ok(json!({"text": "Yes: `x + y` overflows past `u32::MAX`.",
            "suggestion": "let total = x\n    .checked_add(y)?;"}))
    );
}

/// Pi exits 0 whatever happened: its last message's stop reason says
/// whether it failed.
#[test]
fn pi_fails_when_its_last_message_stopped_on_an_error() {
    let f = Fixture::new();
    f.pi();
    f.put(
        "stub/pi/answer.jsonl",
        include_str!("fixtures/pi-error.jsonl"),
    );
    assert_eq!(
        f.ask("pi", "Why?").1,
        Err(json!({"code": 1, "message": "pi failed: 529 overloaded"}))
    );
}

/// Pi retries a failed message itself: a good one after it is the answer.
#[test]
fn pi_answers_after_an_error_it_retried() {
    let f = Fixture::new();
    f.pi();
    f.put(
        "stub/pi/answer.jsonl",
        include_str!("fixtures/pi-retried.jsonl"),
    );
    assert_eq!(
        f.ask("pi", "Why?").1,
        Ok(json!({"text": "It can overflow; `checked_add` says so."}))
    );
}

/// Pi's login check needs a provider named, so it isn't run: a start
/// without a usable model says so on stderr, before any message.
#[test]
fn pi_without_a_key_says_how_to_log_in() {
    let f = Fixture::new();
    f.pi();
    f.put(
        "stub/pi/answer.jsonl",
        "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2026-11-20T18:04:02.021Z\",\"cwd\":\"/work\"}\n",
    );
    f.put(
        "stub/pi/stderr",
        "No API key found for the selected model.\n\n\
         Use /login to log into a provider via OAuth or API key. See:\n  \
         /opt/pi/docs/providers.md\n  /opt/pi/docs/models.md\n",
    );
    f.put("stub/pi/status", "1");
    assert_eq!(
        f.ask("pi", "Why?").1,
        Err(json!({"code": 2,
            "message": "pi isn't logged in; run `pi auth login <provider>`"}))
    );
}

/// Found on PATH, Pi is listed whatever its version; `doctor` shows its
/// version and recipe, and no login check, which needs a provider named.
#[test]
fn pi_on_path_is_listed_with_its_mark() {
    let f = Fixture::new();
    f.pi();
    f.put("stub/pi/version", "0.0.1-dev");
    let (mut session, init) = f.session(None);
    let (_, doctor) = session.call("doctor", &json!({}));
    session.close();
    assert_eq!(
        init["agents"],
        json!([{"name": "pi", "default": true, "mark": "read-only tools"}])
    );
    assert_eq!(
        doctor.expect("checks")["checks"],
        json!([
            {"name": "claude", "ok": true, "detail": "not installed"},
            {"name": "codex", "ok": true, "detail": "not installed"},
            {"name": "pi", "ok": true, "detail": "0.0.1-dev"},
            {"name": "pi recipe", "ok": true,
                "detail": "read-only tools: read, grep, find and ls only, MCP off, \
                           project config ignored, offline; no sandbox"},
        ])
    );
}

/// An agent that isn't installed is news, not a failure, while another
/// one answers; one the config names is a failure.
#[test]
fn doctor_fails_an_agent_not_installed_only_when_its_wanted() {
    let f = Fixture::new();
    f.claude();
    for (agent, ok) in [(None, true), (Some("codex"), false)] {
        let (mut session, _) = f.session(agent);
        let (_, doctor) = session.call("doctor", &json!({}));
        session.close();
        assert_eq!(
            doctor.expect("checks")["checks"],
            json!([
                {"name": "claude", "ok": true, "detail": "2.1.329 (Claude Code)"},
                {"name": "claude recipe", "ok": true,
                    "detail": "read-only: Read, Grep and Glob only, MCP tools denied, user settings only"},
                {"name": "claude auth", "ok": true, "detail": "logged in"},
                {"name": "codex", "ok": ok, "detail": "not installed"},
                {"name": "pi", "ok": true, "detail": "not installed"},
            ]),
            "{agent:?}"
        );
    }
}

/// The version only informs `doctor`: an agent on PATH is listed and runs
/// whatever its `--version` says.
#[test]
fn an_agent_on_path_runs_whatever_its_version() {
    let f = Fixture::new();
    f.codex();
    f.put("stub/codex/version", "dev");
    f.put(
        "stub/codex/answer.jsonl",
        include_str!("fixtures/codex-answer.jsonl"),
    );
    let (mut session, init) = f.session(None);
    assert_eq!(
        init["agents"],
        json!([{"name": "codex", "default": true, "mark": "read-only sandbox"}])
    );
    let (_, answer) = session.call(
        "ask",
        &json!({"agent": "codex", "prompt": "Why?", "cwd": f.work()}),
    );
    let (_, doctor) = session.call("doctor", &json!({}));
    session.close();
    assert_eq!(
        answer,
        Ok(json!({"text": "It can overflow; `checked_add` says so."}))
    );
    assert_eq!(
        doctor.expect("checks")["checks"],
        json!([
            {"name": "claude", "ok": true, "detail": "not installed"},
            {"name": "codex", "ok": true, "detail": "codex-cli dev"},
            {"name": "codex recipe", "ok": true,
                "detail": "read-only sandbox: commands may run, no writes, no network, web search off"},
            {"name": "codex auth", "ok": true, "detail": "logged in"},
            {"name": "pi", "ok": true, "detail": "not installed"},
        ])
    );
}

#[test]
fn a_failed_run_says_why() {
    let f = Fixture::new();
    f.claude().codex();
    f.put(
        "stub/claude/answer.jsonl",
        include_str!("fixtures/claude-error.jsonl"),
    );
    f.put("stub/claude/status", "1");
    f.put(
        "stub/codex/answer.jsonl",
        include_str!("fixtures/codex-failed.jsonl"),
    );
    f.put("stub/codex/status", "1");
    assert_eq!(
        f.ask("claude", "Why?").1,
        Err(json!({"code": 1, "message": "claude failed: API Error: 529 Overloaded"}))
    );
    assert_eq!(
        f.ask("codex", "Why?").1,
        Err(json!({"code": 1, "message": "codex failed: unexpected status 401 Unauthorized"}))
    );
}

#[test]
fn a_failed_run_while_logged_out_says_how_to_log_in() {
    let f = Fixture::new();
    f.claude().codex();
    f.put(
        "stub/codex/answer.jsonl",
        include_str!("fixtures/codex-failed.jsonl"),
    );
    f.put("stub/codex/status", "1");
    f.put("stub/codex/logged-out", "");
    assert_eq!(
        f.ask("codex", "Why?").1,
        Err(json!({"code": 2, "message": "codex isn't logged in; run `codex login`"}))
    );
}

/// A usage error says why on its first line, with a hint and the usage
/// after it, as a codex too old for a recipe's flag does.
#[test]
fn a_usage_error_says_its_error_line() {
    let f = Fixture::new();
    f.codex();
    f.put(
        "stub/codex/stderr",
        "error: unexpected argument '--ephemeral' found\n\n  \
         tip: a similar argument exists: '--experimental-json'\n\n\
         Usage: codex exec --json --sandbox <SANDBOX_MODE> [PROMPT]\n\n\
         For more information, try '--help'.\n",
    );
    f.put("stub/codex/status", "2");
    assert_eq!(
        f.ask("codex", "Why?").1,
        Err(json!({"code": 1,
            "message": "codex failed: error: unexpected argument '--ephemeral' found"}))
    );
}

/// Starts an ask of the sleeping stub claude, and returns its id and the
/// pids of claude and of the `sleep` it started.
fn ask_sleeping(f: &Fixture, session: &mut Session) -> (u64, String) {
    f.put("stub/claude/sleep", "");
    let id = session.request(
        "ask",
        &json!({"agent": "claude", "prompt": "Why?", "cwd": f.work()}),
    );
    let start = Instant::now();
    loop {
        if let Ok(pids) = fs::read_to_string(f.path("stub/claude/pids"))
            && pids.ends_with('\n')
        {
            return (id, pids);
        }
        assert!(start.elapsed() < WAIT, "claude didn't start");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn assert_gone(pids: &str) {
    for pid in pids.split_whitespace() {
        let start = Instant::now();
        while alive(pid) {
            assert!(start.elapsed() < WAIT, "{pid} still runs");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn cancelling_an_ask_kills_the_agent_with_what_it_started() {
    let f = Fixture::new();
    f.claude();
    let (mut session, _) = f.session(None);
    let (id, pids) = ask_sleeping(&f, &mut session);
    session.notify("$/cancelRequest", &json!({"id": id}));
    let (_, answer) = session.answer(id);
    assert_eq!(answer, Err(json!({"code": -32800, "message": "cancelled"})));
    assert_gone(&pids);
    session.close();
}

/// dffy kills the plugin's process group when it quits, which doesn't
/// reach an agent in a group of its own: the plugin kills it as its stdin
/// ends.
#[test]
fn an_ask_running_when_dffy_goes_is_killed() {
    let f = Fixture::new();
    f.claude();
    let (mut session, _) = f.session(None);
    let (_, pids) = ask_sleeping(&f, &mut session);
    drop(session.stdin);
    assert!(session.child.wait().expect("exits").success());
    assert_gone(&pids);
}

/// A configured value with a space is a command of the user's: it gets the
/// prompt on stdin in the worktree, and what it prints is the reply.
#[test]
fn a_custom_command_is_the_default_and_its_stdout_is_the_reply() {
    let f = Fixture::new();
    f.claude()
        .install("my-agent", include_str!("fixtures/stubs/my-agent.sh"));
    f.put(
        "stub/my-agent/answer.txt",
        "Fine as is.\n\n````suggestion\nlet a = b;\n````\n",
    );
    let (mut session, init) = f.session(Some("my-agent --model fast"));
    assert_eq!(
        init["agents"],
        json!([
            {"name": "custom", "default": true, "mark": "custom"},
            {"name": "claude", "default": false, "mark": null},
        ])
    );
    let (_, answer) = session.call(
        "ask",
        &json!({"agent": "custom", "prompt": "Why?", "cwd": f.work()}),
    );
    assert_eq!(
        answer,
        Ok(json!({"text": "Fine as is.", "suggestion": "let a = b;"}))
    );
    assert_eq!(lines(&f.read("stub/my-agent/args")), ["--model", "fast"]);
    assert_eq!(f.read("stub/my-agent/stdin"), "Why?");
    assert_eq!(
        f.read("stub/my-agent/cwd").trim_end(),
        f.work().to_str().expect("utf-8")
    );

    f.put("stub/my-agent/error", "my-agent: no key\n");
    let (_, failed) = session.call(
        "ask",
        &json!({"agent": "custom", "prompt": "Why?", "cwd": f.work()}),
    );
    session.close();
    assert_eq!(
        failed,
        Err(json!({"code": 1, "message": "custom failed: my-agent: no key"}))
    );
}

#[test]
fn a_configured_name_picks_the_default() {
    let f = Fixture::new();
    f.claude().codex();
    let (session, init) = f.session(Some("codex"));
    session.close();
    assert_eq!(
        init["agents"],
        json!([
            {"name": "claude", "default": false, "mark": null},
            {"name": "codex", "default": true, "mark": "read-only sandbox"},
        ])
    );
}

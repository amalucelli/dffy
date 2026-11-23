//! `dffy __navigate`, the navigator helper, end to end: the built binary
//! on the fixture repository, held to the protocol fixture the host side's
//! tests read too, and its lifetime: it exits when idle and when its host
//! goes away.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

#[path = "../src/plugin/json.rs"]
#[expect(dead_code, reason = "only parse and string are used here")]
mod json;

use json::{Value, parse};

const FIXTURE: &str = include_str!("fixtures/navigate-protocol/messages.json");

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs")
        .status
        .success();
    assert!(ok, "git {args:?}");
}

/// `src/frob.h` defines `frob`, which `src/use.c` calls, committed; the
/// working tree adds a second call.
fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    std::fs::create_dir_all(d.join("src")).expect("mkdir");
    std::fs::write(
        d.join("src/frob.h"),
        "static inline int frob(int x)\n{\n\treturn x;\n}\n",
    )
    .expect("write");
    std::fs::write(
        d.join("src/use.c"),
        "#include \"frob.h\"\n\n/* zap */\nint use(void)\n{\n\treturn frob(1);\n}\n",
    )
    .expect("write");
    git(d, &["init", "-q", "-b", "main"]);
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "first"]);
    let mut text = std::fs::read_to_string(d.join("src/use.c")).expect("read");
    text.push_str("int again(void) { return frob(2); }\n");
    std::fs::write(d.join("src/use.c"), text).expect("write");
    dir
}

struct Helper {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next: u32,
    /// What it pushed and the test hasn't looked at yet.
    pushes: Vec<Value>,
}

impl Helper {
    fn start(dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dffy"))
            .arg("__navigate")
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("starts");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
            next: 0,
            pushes: Vec::new(),
        }
    }

    /// `method`'s answer, without its `jsonrpc` and `id`.
    fn ask(&mut self, method: &str, params: &str) -> Value {
        self.send(method, params, false)
    }

    /// `method`'s answer, its `$/cancelRequest` sent right after it.
    fn ask_then_cancel(&mut self, method: &str, params: &str) -> Value {
        self.send(method, params, true)
    }

    fn send(&mut self, method: &str, params: &str, cancel: bool) -> Value {
        self.next += 1;
        let mut line = format!(
            r#"{{"jsonrpc":"2.0","id":{},"method":"{method}","params":{params}}}"#,
            self.next
        );
        if cancel {
            let _ = write!(
                line,
                "\n{{\"jsonrpc\":\"2.0\",\"method\":\"$/cancelRequest\",\"params\":{{\"id\":{}}}}}",
                self.next
            );
        }
        self.write(&line);
        self.answer(&Value::Number(f64::from(self.next)))
    }

    fn write(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("open");
        writeln!(stdin, "{line}").expect("writes");
        stdin.flush().expect("flushes");
    }

    /// The next message the helper writes.
    fn read(&mut self) -> Vec<(String, Value)> {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("reads");
        let Value::Object(fields) = parse(line.as_bytes()).expect("json") else {
            panic!("not an object: {line}");
        };
        fields
    }

    /// The answer with id `id`, without its `jsonrpc` and `id`; the pushes
    /// before it are kept.
    fn answer(&mut self, id: &Value) -> Value {
        loop {
            let fields = self.read();
            let Some((_, got)) = fields.iter().find(|(k, _)| k == "id") else {
                self.pushes.push(Value::Object(fields));
                continue;
            };
            assert_eq!(got, id, "{}", json(&Value::Object(fields.clone())));
            return Value::Object(
                fields
                    .into_iter()
                    .filter(|(k, _)| k != "jsonrpc" && k != "id")
                    .collect(),
            );
        }
    }

    /// Waits for the push `expected`, without its `jsonrpc`.
    fn pushed(&mut self, expected: &Value) {
        let start = Instant::now();
        loop {
            let seen = self
                .pushes
                .drain(..)
                .any(|push| without_version(&push) == *expected);
            if seen {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(30), "never pushed");
            let fields = self.read();
            self.pushes.push(Value::Object(fields));
        }
    }

    /// Whether the process exited within `within`.
    fn exits(&mut self, within: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < within {
            if self.child.try_wait().expect("waits").is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(self.child.kill());
        false
    }
}

fn json(value: &Value) -> String {
    let mut s = String::new();
    json::write(&mut s, value);
    s
}

/// The params as sent: the cache in `cache`.
fn params(message: &Value, cache: &Path) -> String {
    json(message.get("params").expect("params")).replace("@CACHE@", &cache.to_string_lossy())
}

/// The answer the fixture holds: its `result` or `error`.
fn expected(message: &Value) -> Value {
    let Value::Object(fields) = message else {
        panic!("a message isn't an object");
    };
    Value::Object(
        fields
            .iter()
            .filter(|(k, _)| k == "result" || k == "error")
            .cloned()
            .collect(),
    )
}

/// `message` without its `jsonrpc`.
fn without_version(message: &Value) -> Value {
    let Value::Object(fields) = message else {
        return message.clone();
    };
    Value::Object(
        fields
            .iter()
            .filter(|(k, _)| k != "jsonrpc")
            .cloned()
            .collect(),
    )
}

/// Every message of the fixture, sent in order, gets the fixture's answer:
/// a request before `initialize` is refused, the helper pushes how far its
/// build is up to its end, unknown fields are left alone, an unknown method
/// and a line that isn't JSON are answered with their errors.
#[test]
fn the_helper_answers_as_the_fixture_says() {
    let repo = repository();
    let cache = tempfile::tempdir().expect("tempdir");
    let fixture = parse(FIXTURE.as_bytes()).expect("the fixture is JSON");
    let Value::Object(messages) = &fixture else {
        panic!("the fixture isn't an object");
    };
    let mut helper = Helper::start(repo.path());
    for (name, message) in messages {
        if name == "refused" {
            helper = Helper::start(repo.path());
        }
        if name == "pushed" {
            helper.pushed(message);
            continue;
        }
        if name == "malformed" {
            let line = message.get("line").and_then(Value::as_str).expect("line");
            helper.write(line);
            let answer = helper.answer(&Value::Null);
            assert_eq!(json(&answer), json(&expected(message)), "{name}");
            continue;
        }
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .expect("method");
        let answer = helper.ask(method, &params(message, cache.path()));
        assert_eq!(
            fingerprint_hidden(&json(&answer)),
            json(&expected(message)),
            "{name}"
        );
    }
}

/// `answer` with its navigator's fingerprint as the fixture writes it: it
/// takes in the working tree's stat, which differs from run to run.
fn fingerprint_hidden(answer: &str) -> String {
    let key = "\"fingerprint\":\"";
    match answer.find(key) {
        Some(at) => {
            let start = at + key.len();
            let end = start + answer[start..].find('"').unwrap_or(0);
            format!("{}@FINGERPRINT@{}", &answer[..start], &answer[end..])
        }
        None => answer.to_owned(),
    }
}

/// The helper exits once no request came for its idle time, and once its
/// host closes its stdin.
#[test]
fn the_helper_exits_when_idle_and_when_its_host_goes() {
    let repo = repository();
    let cache = tempfile::tempdir().expect("tempdir");
    let init = format!(
        r#"{{"protocol":1,"client":{{"name":"dffy","version":"0"}},"threads":1,"cache":{{"dir":"{}","bytes":1000000}},"idle_ms":300}}"#,
        cache.path().display()
    );
    let mut helper = Helper::start(repo.path());
    helper.ask("initialize", &init);
    assert!(helper.exits(Duration::from_secs(10)), "idle, it exits");

    let mut helper = Helper::start(repo.path());
    drop(helper.stdin.take());
    assert!(
        helper.exits(Duration::from_secs(10)),
        "its host gone, it exits"
    );
}

/// A repository whose `use.c` includes 300 headers of definitions, none
/// indexed yet, and 1500 files that call `frob`, so a lookup and a search
/// take long enough to be stopped.
fn large_repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    for folder in ["inc", "src"] {
        std::fs::create_dir_all(d.join(folder)).expect("mkdir");
    }
    let mut user = String::new();
    for i in 0..300 {
        let mut defs = String::new();
        for j in 0..200 {
            let _ = write!(
                defs,
                "static inline int f{i}_{j}(int x)\n{{\n\treturn x + {j};\n}}\n"
            );
        }
        std::fs::write(d.join(format!("inc/h{i}.h")), defs).expect("write");
        let _ = writeln!(user, "#include \"../inc/h{i}.h\"");
    }
    user.push_str("int use(void) { return frob(1); }\n");
    std::fs::write(d.join("src/use.c"), user).expect("write");
    let filler = "int filler(int x) { return x * 2 + 1; }\n".repeat(400);
    for i in 0..1500 {
        let text = format!("{filler}int g{i}(void) {{ return frob({i}); }}\n");
        std::fs::write(d.join(format!("src/f{i}.c")), text).expect("write");
    }
    git(d, &["init", "-q", "-b", "main"]);
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "first"]);
    dir
}

/// `$/cancelRequest` stops a lookup, which answers error -32800, and a
/// search's step, which answers with what it found so far, the rest left
/// for the next step.
#[test]
fn a_cancelled_lookup_or_step_stops() {
    let repo = large_repository();
    let cache = tempfile::tempdir().expect("tempdir");
    let mut helper = Helper::start(repo.path());
    let init = format!(
        r#"{{"protocol":1,"client":{{"name":"dffy","version":"0"}},"threads":1,"cache":{{"dir":"{}","bytes":100000000}}}}"#,
        cache.path().display()
    );
    helper.ask("initialize", &init);
    let opened = helper.ask(
        "open",
        r#"{"snapshots":[{"kind":"index"},{"kind":"worktree"}],"side":"new","whitespace":"exact"}"#,
    );
    let navigator = (opened.get("result").and_then(|r| r.get("navigator")))
        .and_then(Value::as_usize)
        .expect("a navigator");
    let origin = r#""origin":{"path":"src/use.c","language":"c"}"#;
    let lookup = helper.ask_then_cancel(
        "lookup",
        &format!(r#"{{"navigator":{navigator},"name":"frob",{origin}}}"#),
    );
    let code = lookup.get("error").and_then(|e| e.get("code")).cloned();
    assert_eq!(code, Some(Value::Number(-32800.0)), "{}", json(&lookup));

    let started = helper.ask(
        "search/start",
        &format!(r#"{{"navigator":{navigator},"kind":"references","name":"frob",{origin}}}"#),
    );
    let search = (started.get("result").and_then(|r| r.get("search")))
        .and_then(Value::as_usize)
        .expect("a search");
    let step = helper.ask_then_cancel(
        "search/step",
        &format!(r#"{{"search":{search},"budget_ms":600000}}"#),
    );
    let result = step.get("result").expect("a step answers with a result");
    let searched = result.get("searched").expect("searched");
    let (done, total) = (
        searched
            .get("done")
            .and_then(Value::as_usize)
            .expect("done"),
        searched
            .get("total")
            .and_then(Value::as_usize)
            .expect("total"),
    );
    assert_eq!(result.get("done"), Some(&Value::Bool(false)));
    assert!(done < total, "stopped at {done} of {total} files");
}

/// A running index build holds off the idle exit: the helper exits once
/// its build ended and it pushed so.
#[test]
fn an_index_build_holds_off_the_idle_exit() {
    let repo = large_repository();
    let cache = tempfile::tempdir().expect("tempdir");
    let mut helper = Helper::start(repo.path());
    let init = format!(
        r#"{{"protocol":1,"client":{{"name":"dffy","version":"0"}},"threads":1,"cache":{{"dir":"{}","bytes":100000000}},"idle_ms":100}}"#,
        cache.path().display()
    );
    helper.ask("initialize", &init);
    helper.ask(
        "open",
        r#"{"snapshots":[{"kind":"index"},{"kind":"worktree"}],"side":"new","whitespace":"exact"}"#,
    );
    assert!(
        helper.exits(Duration::from_secs(120)),
        "it exits once built"
    );
    let mut last = helper.pushes.pop();
    let mut line = String::new();
    while helper.stdout.read_line(&mut line).expect("reads") > 0 {
        last = Some(parse(line.as_bytes()).expect("json"));
        line.clear();
    }
    let params = last.and_then(|l| l.get("params").cloned()).expect("a push");
    let progress = params.get("progress").expect("progress");
    assert_eq!(
        progress.get("done"),
        progress.get("total"),
        "{}",
        json(&params)
    );
    assert_eq!(params.get("building"), Some(&Value::Bool(false)));
}

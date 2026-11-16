//! `dffy mcp` end to end: the binary on a fixture repository, spoken to
//! over stdin and stdout in both eras of the protocol, its answers read
//! with dffy's own JSON parser.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

#[path = "../src/plugin/json.rs"]
mod json;

use json::{Value, parse, string};

const MODERN_META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}"#;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada Lovelace")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .env("GIT_AUTHOR_DATE", "2024-03-01T12:00:00+0100")
        .env("GIT_COMMITTER_DATE", "2024-03-01T12:00:00+0100")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(dir: &Path, path: &str, content: &str) {
    let full = dir.join(path);
    std::fs::create_dir_all(full.parent().expect("has parent")).expect("mkdir");
    std::fs::write(full, content).expect("write");
}

/// A commit with an edit, a rename, an addition, two functions swapped,
/// spacing changed and nothing else, and a file with two long hunks.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    write(
        d,
        "lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    );
    write(d, "docs/guide.md", "# Guide\n\nRead me.\n");
    let add = "int add(int a, int b)\n{\n\treturn a + b;\n}\n";
    let sub = "int sub(int a, int b)\n{\n\treturn a - b;\n}\n";
    write(d, "math.c", &format!("{add}\n{sub}"));
    write(d, "spacing.c", "int x = 1;\nint y = 2;\nint z = 3;\n");
    write(d, "long.txt", &long_file(|_| false));
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    write(
        d,
        "lib.rs",
        "pub fn add(a: i64, b: i64) -> i64 {\n    a + b\n}\n",
    );
    std::fs::create_dir_all(d.join("docs/user")).expect("mkdir");
    std::fs::rename(d.join("docs/guide.md"), d.join("docs/user/guide.md")).expect("mv");
    write(d, "new.txt", "brand new\nno newline");
    write(d, "math.c", &format!("{sub}\n{add}"));
    write(d, "spacing.c", "int x = 1;\nint  y=2;\nint z = 3;\n");
    write(d, "long.txt", &long_file(|i| matches!(i, 3..=8 | 30..=35)));
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "change"]);
    dir
}

/// Forty numbered lines, the ones `changed` picks saying so.
fn long_file(changed: impl Fn(usize) -> bool) -> String {
    let mut s = String::new();
    for i in 1..=40 {
        let word = if changed(i) { "changed" } else { "line" };
        let _ = writeln!(s, "{word} {i}");
    }
    s
}

/// `dffy mcp` running in a repository, with an empty config, state and
/// cache so the user's can't leak in.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: usize,
}

impl Server {
    fn start(dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dffy"))
            .current_dir(dir)
            .arg("mcp")
            .env("XDG_CONFIG_HOME", dir.join(".no-config"))
            .env("XDG_CACHE_HOME", dir.join(".git/dffy-test-cache"))
            .env("XDG_STATE_HOME", dir.join(".git/dffy-test-state"))
            .env("XDG_RUNTIME_DIR", dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn dffy mcp");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn send(&mut self, message: &str) {
        let stdin = self.stdin.as_mut().expect("open");
        writeln!(stdin, "{message}").expect("send");
        stdin.flush().expect("flush");
    }

    /// The next message from the server, as its line and its value. Every
    /// message is one line of JSON.
    fn receive(&mut self) -> (String, Value) {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read");
        assert!(line.ends_with('\n'), "a message ends its line: {line}");
        line.pop();
        let value = parse(line.as_bytes()).unwrap_or_else(|e| panic!("{e}: {line}"));
        (line, value)
    }

    /// Sends request `method` with `params` (JSON object members) and
    /// returns its response, after checking it answers this request.
    fn request(&mut self, method: &str, params: &str) -> (String, Value) {
        self.next_id += 1;
        let id = self.next_id;
        let mut m = String::new();
        string(&mut m, method);
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":{m},"params":{{{params}}}}}"#
        ));
        let (line, value) = self.receive();
        assert_eq!(
            value.get("id").and_then(Value::as_usize),
            Some(id),
            "{line}"
        );
        (line, value)
    }

    /// Calls `tool` with `arguments` (a JSON object), in the legacy era.
    fn call(&mut self, tool: &str, arguments: &str) -> Call {
        let (line, value) = self.request(
            "tools/call",
            &format!(r#""name":"{tool}","arguments":{arguments}"#),
        );
        let result = value
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{line}"));
        Call { line, result }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Closing stdin is the shutdown signal.
        drop(self.stdin.take());
        let status = self.child.wait().expect("wait");
        assert!(status.success() || std::thread::panicking(), "{status}");
    }
}

struct Call {
    line: String,
    result: Value,
}

impl Call {
    fn is_error(&self) -> bool {
        self.result.get("isError") == Some(&Value::Bool(true))
    }

    fn structured(&self) -> &Value {
        assert!(!self.is_error(), "{}", self.text());
        self.result.get("structuredContent").expect("structured")
    }

    fn text(&self) -> &str {
        self.result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .expect("text content")
    }

    /// The structured content as sent.
    fn raw_structured(&self) -> String {
        let start = self
            .line
            .find(r#""structuredContent":"#)
            .expect("structured")
            + 20;
        let end = self.line.rfind(r#","isError":false"#).expect("isError");
        self.line[start..end].to_owned()
    }

    fn cursor(&self) -> Option<&str> {
        self.structured().get("next_cursor").and_then(Value::as_str)
    }
}

/// `parts` for a snapshot, with cursors replaced so it holds no hashes.
fn snapshot(parts: &[String]) -> String {
    redact(&parts.join("\n--\n"))
}

/// `s` with every cursor (16 hex digits, a dot, a number, a dot, a
/// number) replaced by `<cursor>`.
fn redact(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(|c: char| c.is_ascii_hexdigit()) {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let hex = rest.bytes().take_while(u8::is_ascii_hexdigit).count();
        let tail = &rest[hex..];
        let numbers = |t: &str| {
            let t = t.strip_prefix('.')?;
            let n = t.bytes().take_while(u8::is_ascii_digit).count();
            (n > 0).then_some(n + 1)
        };
        let cursor = (hex == 16)
            .then(|| numbers(tail))
            .flatten()
            .and_then(|a| numbers(&tail[a..]).map(|b| a + b));
        if let Some(len) = cursor {
            out.push_str("<cursor>");
            rest = &rest[hex + len..];
        } else {
            out.push_str(&rest[..hex]);
            rest = &rest[hex..];
        }
    }
    out.push_str(rest);
    out
}

fn initialize(server: &mut Server) {
    let (line, value) = server.request(
        "initialize",
        r#""protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}"#,
    );
    let result = value.get("result").unwrap_or_else(|| panic!("{line}"));
    assert_eq!(
        result.get("protocolVersion").and_then(Value::as_str),
        Some("2025-11-25")
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|c| c.get("tools"))
            .is_some()
    );
    // A notification gets no answer: the next line answers the next request.
    server.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
}

/// The handshake, the tool list, a page of changed files and the next,
/// and `dffy --json`'s file objects in the results.
#[test]
fn changed_files_pages_through_the_list_after_the_handshake() {
    let repo = repo();
    let mut server = Server::start(repo.path());
    initialize(&mut server);
    let (_, pong) = server.request("ping", "");
    assert_eq!(pong.get("result"), Some(&Value::Object(Vec::new())));
    let (line, tools) = server.request("tools/list", "");
    let names: Vec<_> = tools
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{line}"))
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    assert_eq!(names, ["changed_files", "file_diff", "diff"]);

    let range = r#""range":"main~1 main""#;
    let first = server.call("changed_files", &format!("{{{range},\"limit\":4}}"));
    let cursor = first.cursor().expect("more files").to_owned();
    let second = server.call(
        "changed_files",
        &format!("{{{range},\"limit\":4,\"cursor\":\"{cursor}\"}}"),
    );
    assert_eq!(second.cursor(), None, "the second page is the last");
    let paths = |call: &Call| -> Vec<String> {
        call.structured()
            .get("files")
            .and_then(Value::as_array)
            .expect("files")
            .iter()
            .filter_map(|f| f.get("path").and_then(Value::as_str).map(str::to_owned))
            .collect()
    };
    let mut all = paths(&first);
    all.extend(paths(&second));
    assert_eq!(
        all,
        [
            "docs/user/guide.md",
            "lib.rs",
            "long.txt",
            "math.c",
            "new.txt",
            "spacing.c"
        ]
    );
    let enumerated = server.call("changed_files", &format!("{{{range},\"counts\":false}}"));
    insta::assert_snapshot!(
        "mcp_changed_files",
        snapshot(&[
            first.raw_structured(),
            first.text().to_owned(),
            enumerated.raw_structured(),
        ])
    );
}

/// A file cut short continues from its cursor until the end, and the parts
/// hold every line of the whole file once, with hunk numbers that describe
/// the lines they hold.
#[test]
fn file_diff_continues_a_truncated_file_to_its_end() {
    let repo = repo();
    let mut server = Server::start(repo.path());
    initialize(&mut server);
    let args = |cursor: Option<&str>| {
        let cursor = cursor
            .map(|c| format!(",\"cursor\":\"{c}\""))
            .unwrap_or_default();
        format!(r#"{{"path":"long.txt","range":["main~1","main"],"max_lines":5{cursor}}}"#)
    };
    let whole = server.call("file_diff", r#"{"path":"long.txt","range":"main~1 main"}"#);
    let lines = |call: &Call| -> Vec<Value> {
        let file = call.structured().get("file").expect("file");
        file.get("hunks")
            .and_then(Value::as_array)
            .expect("hunks")
            .iter()
            .flat_map(|h| {
                let lines = h.get("lines").and_then(Value::as_array).expect("lines");
                let count = |kind: &str| {
                    lines
                        .iter()
                        .filter(|l| l.get("kind").and_then(Value::as_str) != Some(kind))
                        .count()
                };
                assert_eq!(
                    h.get("old_lines").and_then(Value::as_usize),
                    Some(count("added"))
                );
                assert_eq!(
                    h.get("new_lines").and_then(Value::as_usize),
                    Some(count("removed"))
                );
                // A side's start is its first line's number, when it has one.
                for side in ["old", "new"] {
                    let first = lines
                        .iter()
                        .find_map(|l| l.get(side).and_then(Value::as_usize));
                    if first.is_some() {
                        let start = h.get(&format!("{side}_start")).and_then(Value::as_usize);
                        assert_eq!(start, first, "{side}_start of {}", call.line);
                    }
                }
                lines.to_vec()
            })
            .collect()
    };
    assert_eq!(
        whole.structured().get("truncated"),
        Some(&Value::Bool(false))
    );
    let mut parts = Vec::new();
    let mut joined = Vec::new();
    let mut cursor = None;
    loop {
        let part = server.call("file_diff", &args(cursor.as_deref()));
        let got = lines(&part);
        assert!(!got.is_empty() && got.len() <= 5, "{}", part.line);
        joined.extend(got);
        cursor = part.cursor().map(str::to_owned);
        parts.push(part);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(joined, lines(&whole));
    insta::assert_snapshot!(
        "mcp_file_diff",
        snapshot(&[
            parts[0].raw_structured(),
            parts[0].text().to_owned(),
            parts[1].text().to_owned(),
        ])
    );
    // The cursor continues only its own request.
    let first = parts[0].cursor().expect("cut short");
    let other = server.call(
        "file_diff",
        &format!(r#"{{"path":"long.txt","range":"main~1 main","context":1,"cursor":"{first}"}}"#),
    );
    assert!(other.is_error());
    assert!(other.text().contains("start over"), "{}", other.text());
}

/// `diff` shows several files; with `syntax: true` swapped functions are
/// moved and the file `moved_only`, spacing changes `formatting_only`.
#[test]
fn diff_classifies_files_by_syntax() {
    let repo = repo();
    let mut server = Server::start(repo.path());
    initialize(&mut server);
    let call = server.call(
        "diff",
        r#"{"range":"main~1 main","paths":["math.c","spacing.c"],"syntax":true}"#,
    );
    let classes: Vec<_> = call
        .structured()
        .get("files")
        .and_then(Value::as_array)
        .expect("files")
        .iter()
        .map(|f| f.get("classification").and_then(Value::as_str))
        .collect();
    assert_eq!(classes, [Some("moved_only"), Some("formatting_only")]);
    insta::assert_snapshot!(
        "mcp_diff_syntax",
        snapshot(&[call.raw_structured(), call.text().to_owned()])
    );
    let cut = server.call(
        "diff",
        r#"{"range":"main~1 main","paths":"long.txt","max_lines_per_file":3}"#,
    );
    let truncated = cut
        .structured()
        .get("truncated_files")
        .and_then(Value::as_array)
        .expect("truncated_files");
    let cursor = truncated[0]
        .get("cursor")
        .and_then(Value::as_str)
        .expect("cursor");
    let rest = server.call(
        "file_diff",
        &format!(r#"{{"path":"long.txt","range":"main~1 main","cursor":"{cursor}"}}"#),
    );
    assert_eq!(
        rest.structured().get("truncated"),
        Some(&Value::Bool(false))
    );
}

/// Bad arguments are tool errors the model can read; a tool that doesn't
/// exist is a protocol error.
#[test]
fn invalid_input_is_a_tool_error() {
    let repo = repo();
    let mut server = Server::start(repo.path());
    initialize(&mut server);
    for (tool, args, says) in [
        ("file_diff", "{}", "path is required"),
        ("file_diff", r#"{"path":"missing.rs"}"#, "has no changes"),
        (
            "file_diff",
            r#"{"path":"docs","range":"main~1 main"}"#,
            "is a directory",
        ),
        ("changed_files", r#"{"limit":0}"#, "limit must be"),
        ("changed_files", r#"{"range":"nope"}"#, "nope"),
        ("changed_files", r#"{"cursor":"garbage"}"#, "start over"),
        ("diff", r#"{"max_line":3}"#, "unknown argument"),
    ] {
        let call = server.call(tool, args);
        assert!(call.is_error(), "{tool} {args}: {}", call.line);
        assert!(call.text().contains(says), "{tool} {args}: {}", call.text());
        assert!(call.result.get("structuredContent").is_none());
    }
    let (line, unknown) = server.request("tools/call", r#""name":"nope","arguments":{}"#);
    let code = unknown.get("error").and_then(|e| e.get("code")).cloned();
    assert_eq!(code, Some(Value::Number(-32602.0)), "{line}");
}

/// Without the handshake every request carries its version: discovery
/// answers with the versions served, a call gets `resultType`, a request
/// without the version is refused, and an unknown version names the ones
/// there are.
#[test]
fn modern_requests_carry_their_version() {
    let repo = repo();
    let mut server = Server::start(repo.path());
    let (line, discover) = server.request("server/discover", MODERN_META);
    let result = discover.get("result").unwrap_or_else(|| panic!("{line}"));
    let versions: Vec<_> = result
        .get("supportedVersions")
        .and_then(Value::as_array)
        .expect("versions")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(versions, ["2026-07-28", "2025-11-25", "2025-06-18"]);
    assert_eq!(
        result.get("resultType").and_then(Value::as_str),
        Some("complete")
    );
    assert!(result.get("ttlMs").is_some() && result.get("cacheScope").is_some());

    let (line, call) = server.request(
        "tools/call",
        &format!(r#"{MODERN_META},"name":"changed_files","arguments":{{"range":"main~1 main"}}"#),
    );
    let result = call.get("result").unwrap_or_else(|| panic!("{line}"));
    assert_eq!(
        result.get("resultType").and_then(Value::as_str),
        Some("complete")
    );
    assert_eq!(
        result
            .get("structuredContent")
            .and_then(|s| s.get("total_files"))
            .and_then(Value::as_usize),
        Some(6)
    );

    let error = |value: &Value| value.get("error").and_then(|e| e.get("code")).cloned();
    let (line, bare) = server.request("tools/list", "");
    assert_eq!(error(&bare), Some(Value::Number(-32602.0)), "{line}");
    let (line, old) = server.request(
        "tools/list",
        r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2024-01-01","io.modelcontextprotocol/clientCapabilities":{}}"#,
    );
    assert_eq!(error(&old), Some(Value::Number(-32022.0)), "{line}");
    let requested = old
        .get("error")
        .and_then(|e| e.get("data"))
        .and_then(|d| d.get("requested"))
        .and_then(Value::as_str);
    assert_eq!(requested, Some("2024-01-01"));
}

/// The repository stays open across calls, and each call sees it as it is
/// now: a file written and a commit made after startup show on the next
/// call.
#[test]
fn each_call_sees_the_repository_as_it_is_now() {
    let repo = repo();
    let d = repo.path();
    let mut server = Server::start(d);
    initialize(&mut server);
    let total = |server: &mut Server, args: &str| {
        server
            .call("changed_files", args)
            .structured()
            .get("total_files")
            .and_then(Value::as_usize)
    };
    assert_eq!(total(&mut server, "{}"), Some(0));
    write(d, "later.txt", "written after startup\n");
    assert_eq!(total(&mut server, "{}"), Some(1));
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "later"]);
    assert_eq!(total(&mut server, "{}"), Some(0));
    assert_eq!(total(&mut server, r#"{"range":"HEAD~1"}"#), Some(1));
}

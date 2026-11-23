//! `dffy mcp` end to end: the binary on a fixture repository, spoken to
//! over stdin and stdout in both eras of the protocol, its answers read
//! with dffy's own JSON parser.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "fixture helpers fail the test at the step that broke"
)]

mod common;

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

#[path = "../src/plugin/json.rs"]
#[expect(
    dead_code,
    reason = "requests are built as text, so no value is written"
)]
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
        Self::start_in(dir, dir)
    }

    /// With `runtime` as where dffyd is, to reach the viewers there.
    fn start_in(dir: &Path, runtime: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dffy"))
            .current_dir(dir)
            .arg("mcp")
            .env("XDG_CONFIG_HOME", dir.join(".no-config"))
            .env("XDG_CACHE_HOME", dir.join(".git/dffy-test-cache"))
            .env("XDG_STATE_HOME", dir.join(".git/dffy-test-state"))
            .env("XDG_RUNTIME_DIR", runtime)
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

/// `since_checkpoint` shows only what changed since a checkpoint, the
/// latest or one by number, in place of `range`; with `range` it's refused.
#[test]
fn since_checkpoint_shows_only_what_changed_after_it() {
    let repo = repo();
    let d = repo.path();
    write(d, "lib.rs", "// before the checkpoint\n");
    let taken = Command::new(env!("CARGO_BIN_EXE_dffy"))
        .current_dir(d)
        .arg("checkpoint")
        .env("XDG_CONFIG_HOME", d.join(".no-config"))
        .output()
        .expect("dffy checkpoint");
    assert!(taken.status.success(), "{taken:?}");
    write(d, "math.c", "// after it\n");
    let mut server = Server::start(d);
    initialize(&mut server);
    let paths = |call: &Call| -> Vec<String> {
        (call.structured().get("files").and_then(Value::as_array))
            .expect("files")
            .iter()
            .filter_map(|f| f.get("path").and_then(Value::as_str).map(str::to_owned))
            .collect()
    };
    let latest = server.call("changed_files", r#"{"since_checkpoint":"latest"}"#);
    assert_eq!(paths(&latest), ["math.c"]);
    assert!(
        latest.text().contains("since the latest checkpoint"),
        "{}",
        latest.text()
    );
    let one = server.call("file_diff", r#"{"path":"math.c","since_checkpoint":1}"#);
    assert!(one.text().contains("+ // after it"), "{}", one.text());
    let both = server.call(
        "changed_files",
        r#"{"since_checkpoint":"latest","range":"HEAD"}"#,
    );
    assert!(
        both.is_error() && both.text().contains("range"),
        "{}",
        both.text()
    );
    let gone = server.call("changed_files", r#"{"since_checkpoint":9}"#);
    assert!(gone.is_error(), "{}", gone.text());
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
    assert_eq!(
        names,
        [
            "changed_files",
            "file_diff",
            "diff",
            "list_comments",
            "thread_prompt",
            "reply_to_thread",
            "apply_suggestion",
            "open_in_viewer",
            "focus_thread",
            "viewer_selection",
            "definitions"
        ]
    );

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
        ("diff", r#"{"session":true}"#, "needs comments: true"),
        (
            "list_comments",
            r#"{"path":["a"]}"#,
            "path must be a string",
        ),
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

/// `list_comments` reads the repository's comments file on each call:
/// threads in path order, each comment's object as the file has it, a page
/// at a time; `path` keeps one file's, by its old path too, and a cursor
/// from before the file changed is refused.
#[test]
fn list_comments_pages_through_threads() {
    let repo = repo();
    let d = repo.path();
    let comments = |extra: &str| {
        format!(
            r#"{{"version":1,"notice":"comments as of 10:42","comments":[
{{"id":"c0000000000000a1","path":"lib.rs","side":"new","start":{{"old":null,"new":1}},"end":{{"old":null,"new":1}},"text":"Why i64?","author":"","created":"2026-11-17T10:42:00Z","resolved":false,"anchor":{{"before":[],"lines":["pub fn add(a: i64, b: i64) -> i64 {{"],"after":["    a + b","}}"]}},"distance":0}},
{{"id":"c0000000000000b2","path":"docs/user/guide.md","old_path":"docs/guide.md","text":"Moved?","author":"octocat","created":"2026-11-17T10:43:00Z","resolved":true}},
{{"id":"c0000000000000c3","path":"lib.rs","text":"It overflowed.","author":"octocat","created":"2026-11-17T10:44:00Z","in_reply_to":"c0000000000000a1","resolved":false,"ref":"github:PRRC_1"}}{extra}
]}}
"#
        )
    };
    write(d, ".git/dffy/comments.json", &comments(""));
    let mut server = Server::start(d);
    initialize(&mut server);
    let ids = |call: &Call| -> Vec<String> {
        call.structured()
            .get("comments")
            .and_then(Value::as_array)
            .expect("comments")
            .iter()
            .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect()
    };
    let first = server.call("list_comments", r#"{"limit":2}"#);
    assert_eq!(ids(&first), ["c0000000000000b2", "c0000000000000a1"]);
    let cursor = first.cursor().expect("more comments").to_owned();
    let second = server.call(
        "list_comments",
        &format!(r#"{{"limit":2,"cursor":"{cursor}"}}"#),
    );
    assert_eq!(ids(&second), ["c0000000000000c3"]);
    assert_eq!(second.cursor(), None);
    insta::assert_snapshot!(
        "mcp_list_comments",
        snapshot(&[
            first.raw_structured(),
            first.text().to_owned(),
            second.text().to_owned(),
        ])
    );
    let lib = server.call("list_comments", r#"{"path":"lib.rs"}"#);
    assert_eq!(ids(&lib), ["c0000000000000a1", "c0000000000000c3"]);
    let renamed = server.call("list_comments", r#"{"path":"docs/guide.md"}"#);
    assert_eq!(ids(&renamed), ["c0000000000000b2"]);

    write(
        d,
        ".git/dffy/comments.json",
        &comments(
            r#",{"id":"c0000000000000d4","path":"a.rs","text":"New.","author":"","created":"2026-11-17T10:45:00Z","resolved":false}"#,
        ),
    );
    let stale = server.call(
        "list_comments",
        &format!(r#"{{"limit":2,"cursor":"{cursor}"}}"#),
    );
    assert!(stale.is_error());
    assert!(stale.text().contains("start over"), "{}", stale.text());
}

/// `thread_prompt` gives the thread of any of its comments as one prompt,
/// placed on the uncommitted changes, with the file's hunk near it; an id
/// no comment has is a tool error.
#[test]
fn thread_prompt_gives_a_thread_with_its_lines_and_changes() {
    let repo = repo();
    let d = repo.path();
    write(
        d,
        ".git/dffy/comments.json",
        r#"{"version":1,"comments":[
{"id":"c1","path":"lib.rs","side":"new","start":{"old":null,"new":1},"end":{"old":null,"new":1},"text":"Why i64?","author":"ana","created":"t1","resolved":false,"anchor":{"before":[],"lines":["pub fn add(a: i64, b: i64) -> i64 {"],"after":["    a + b","}"]}},
{"id":"c2","path":"lib.rs","text":"It overflowed.","author":"","created":"t2","in_reply_to":"c1","resolved":false}
]}"#,
    );
    write(
        d,
        "lib.rs",
        "pub fn add(a: i64, b: i64) -> i64 {\n    a.wrapping_add(b)\n}\n",
    );
    let mut server = Server::start(d);
    initialize(&mut server);
    let call = server.call("thread_prompt", r#"{"id":"c2"}"#);
    let structured = call.structured();
    let field = |key: &str| structured.get(key).and_then(Value::as_str);
    assert_eq!(
        (field("thread"), field("path")),
        (Some("c1"), Some("lib.rs"))
    );
    assert_eq!(structured.get("trimmed"), Some(&Value::Bool(false)));
    let prompt = field("prompt").expect("a prompt");
    assert_eq!(prompt, call.text());
    for part in [
        "A review thread on lib.rs:1 (the new version of the file).",
        "ana:\nWhy i64?",
        "you:\nIt overflowed.",
        "> 1  pub fn add(a: i64, b: i64) -> i64 {",
        "+    a.wrapping_add(b)",
        "Answer the thread's last comment.",
    ] {
        assert!(prompt.contains(part), "{part}: {prompt}");
    }
    let missing = server.call("thread_prompt", r#"{"id":"nope"}"#);
    assert!(missing.is_error());
    assert!(
        missing.text().contains("No comment has the id nope"),
        "{}",
        missing.text()
    );
}

/// `list_comments` takes the current branch's threads and those on none,
/// or another branch's or all; open or resolved ones, judged by the
/// thread's first comment; and the review session's file with `session`.
#[test]
fn list_comments_filters_by_branch_and_state() {
    let repo = repo();
    let d = repo.path();
    write(
        d,
        ".git/dffy/comments.json",
        r#"{"version":1,"comments":[
{"id":"m1","path":"lib.rs","text":"Open on main.","author":"","created":"t1","resolved":false,"branch":"main"},
{"id":"m2","path":"lib.rs","text":"Done on main.","author":"","created":"t2","resolved":true,"branch":"main"},
{"id":"m3","path":"lib.rs","text":"Reply.","author":"ada","created":"t3","in_reply_to":"m2","resolved":false,"branch":"main"},
{"id":"t1","path":"lib.rs","text":"On topic.","author":"","created":"t4","resolved":false,"branch":"topic"}
]}"#,
    );
    let mut server = Server::start(d);
    initialize(&mut server);
    let ids = |call: &Call| -> Vec<String> {
        assert!(!call.is_error(), "{}", call.text());
        call.structured()
            .get("comments")
            .and_then(Value::as_array)
            .expect("comments")
            .iter()
            .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect()
    };
    let here = server.call("list_comments", "{}");
    assert_eq!(ids(&here), ["m1", "m2", "m3"]);
    let resolved = server.call("list_comments", r#"{"resolved":true}"#);
    assert_eq!(ids(&resolved), ["m2", "m3"]);
    let open = server.call("list_comments", r#"{"open":true}"#);
    assert_eq!(ids(&open), ["m1"]);
    assert_eq!(
        ids(&server.call("list_comments", r#"{"resolved":false}"#)),
        ["m1"]
    );
    let topic = server.call("list_comments", r#"{"branch":"topic"}"#);
    assert_eq!(ids(&topic), ["t1"]);
    let all = server.call("list_comments", r#"{"all_branches":true,"open":true}"#);
    assert_eq!(ids(&all), ["m1", "t1"]);
    insta::assert_snapshot!(
        "mcp_list_comments_filtered",
        snapshot(&[all.raw_structured(), all.text().to_owned()])
    );
    for bad in [
        r#"{"open":true,"resolved":true}"#,
        r#"{"branch":"topic","all_branches":true}"#,
        r#"{"session":true}"#,
    ] {
        let call = server.call("list_comments", bad);
        assert!(call.is_error(), "{bad}");
    }

    let pr = d.join(".git/review/7.json");
    write(
        d,
        ".git/review/7.json",
        r#"{"version":1,"comments":[{"id":"p1","path":"lib.rs","text":"Posted.","author":"octocat","created":"t1","resolved":false,"ref":"github:1"},
{"id":"p2","path":"lib.rs","text":"Mine.","author":"","created":"t2","in_reply_to":"p1","resolved":false},
{"id":"p3","path":"lib.rs","text":"An answer.","author":"claude","created":"t3","in_reply_to":"p1","resolved":false}]}"#,
    );
    write(
        d,
        ".git/dffy/session",
        &format!(
            r#"{{"version":1,"comments":"{}","review":"7","branch":"main","head":"abc1234"}}"#,
            pr.display()
        ),
    );
    let session = server.call("list_comments", r#"{"session":true}"#);
    assert_eq!(ids(&session), ["p1", "p2", "p3"]);
    let standing = |call: &Call, id: &str| {
        (call.structured().get("standing"))
            .and_then(|s| s.get(id))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    assert_eq!(
        ["p1", "p2", "p3"].map(|id| standing(&session, id)),
        [
            Some("posted".to_owned()),
            Some("draft".to_owned()),
            Some("local".to_owned())
        ],
        "where each lives in a review session"
    );
    assert_eq!(standing(&here, "m1"), None, "nothing is posted from here");
}

/// The local review's file of the fixture: a thread with a reply on an
/// added line of `lib.rs`, one on `long.txt` whose lines are gone, and a
/// comment on the whole of `math.c`.
fn placed_comments(d: &Path) {
    write(
        d,
        ".git/dffy/comments.json",
        r#"{"version":1,"comments":[
{"id":"a1","path":"lib.rs","side":"new","start":{"old":null,"new":1},"end":{"old":null,"new":1},"text":"Why i64?","author":"ada","created":"2024-03-01T12:00:00Z","resolved":false,"anchor":{"before":[],"lines":["pub fn add(a: i64, b: i64) -> i64 {"],"after":[]}},
{"id":"a2","path":"lib.rs","text":"It overflowed.\nTwice.","author":"","created":"2024-03-02T12:00:00Z","in_reply_to":"a1","resolved":false},
{"id":"l1","path":"long.txt","side":"old","start":{"old":4,"new":null},"end":{"old":4,"new":null},"text":"Gone.","author":"","created":"2024-03-01T12:00:00Z","resolved":false,"anchor":{"before":[],"lines":["nowhere"],"after":[]}},
{"id":"m1","path":"math.c","text":"On the file.","author":"","created":"2024-03-01T12:00:00Z","resolved":false}
]}"#,
    );
}

/// With `comments: true`, `file_diff` and `diff` give each file object its
/// threads' annotations, as `dffy --json --show-comments` does, and say
/// them in the text; without it the objects have none.
#[test]
fn diff_tools_add_annotations_with_comments() {
    let repo = repo();
    let d = repo.path();
    placed_comments(d);
    let mut server = Server::start(d);
    initialize(&mut server);
    let file = server.call(
        "file_diff",
        r#"{"path":"lib.rs","range":"main~1 main","comments":true}"#,
    );
    insta::assert_snapshot!(
        "mcp_file_diff_comments",
        snapshot(&[file.raw_structured(), file.text().to_owned()])
    );
    let annotated = |call: &Call| -> Vec<Option<usize>> {
        call.structured()
            .get("files")
            .and_then(Value::as_array)
            .expect("files")
            .iter()
            .map(|f| {
                f.get("annotations")
                    .and_then(Value::as_array)
                    .map(<[Value]>::len)
            })
            .collect()
    };
    let both = server.call(
        "diff",
        r#"{"range":"main~1 main","paths":["lib.rs","long.txt","math.c"],"comments":true}"#,
    );
    assert_eq!(annotated(&both), [Some(1), Some(1), Some(0)]);
    assert!(both.text().contains("lines not found"), "{}", both.text());
    let plain = server.call(
        "diff",
        r#"{"range":"main~1 main","paths":["lib.rs","long.txt","math.c"]}"#,
    );
    assert_eq!(annotated(&plain), [None, None, None]);

    // A file cut short has them in its first part only.
    let args = r#""path":"long.txt","range":"main~1 main","comments":true,"max_lines":1"#;
    let annotations = |call: &Call| {
        (call.structured().get("file"))
            .and_then(|f| f.get("annotations"))
            .and_then(Value::as_array)
            .map(<[Value]>::len)
    };
    let first = server.call("file_diff", &format!("{{{args}}}"));
    assert_eq!(annotations(&first), Some(1));
    let cursor = first.cursor().expect("cut short").to_owned();
    let next = server.call("file_diff", &format!(r#"{{{args},"cursor":"{cursor}"}}"#));
    assert!(!next.is_error(), "{}", next.text());
    assert_eq!(annotations(&next), None);
}

/// `list_comments` gives each line thread's place on the range's diff,
/// by its first comment's id, when its file is in it: with `range`, or
/// on the uncommitted changes with `placed`. Otherwise nothing is diffed.
#[test]
fn list_comments_places_threads_on_the_range() {
    let repo = repo();
    let d = repo.path();
    placed_comments(d);
    let mut server = Server::start(d);
    initialize(&mut server);
    // The `placed` member, as sent.
    let placed = |call: &Call| {
        let raw = call.raw_structured();
        let start = raw.find(r#""placed":"#).expect("placed") + 9;
        let end = raw.find(r#","standing""#).expect("standing");
        raw[start..end].to_owned()
    };
    let uncommitted = server.call("list_comments", r#"{"placed":true}"#);
    assert_eq!(placed(&uncommitted), "{}", "nothing uncommitted");
    let range = server.call("list_comments", r#"{"range":"main~1 main"}"#);
    insta::assert_snapshot!(
        "mcp_list_comments_placed",
        snapshot(&[range.raw_structured(), range.text().to_owned()])
    );
    write(
        d,
        "lib.rs",
        "// moved down\npub fn add(a: i64, b: i64) -> i64 {\n    a + b\n}\n",
    );
    let unasked = server.call("list_comments", r#"{"path":"lib.rs"}"#);
    assert_eq!(placed(&unasked), "{}", "not asked for");
    let moved = server.call("list_comments", r#"{"path":"lib.rs","placed":true}"#);
    assert_eq!(
        placed(&moved),
        r#"{"a1":{"side":"new","start":2,"end":2,"lost":false}}"#
    );
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

/// A viewer in a pseudo-terminal, its output drained so it never blocks;
/// killed on drop.
struct Viewer {
    _master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Viewer {
    /// `dffy args` in `dir`, registered with the dffyd at `runtime` once
    /// this returns.
    fn start(dir: &Path, runtime: &Path, args: &[&str]) -> Self {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("a pty");
        let mut cmd = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_dffy"));
        cmd.args(args);
        cmd.cwd(dir);
        cmd.env("TERM", "xterm-256color");
        cmd.env("XDG_CONFIG_HOME", dir.join(".no-config"));
        cmd.env("XDG_CACHE_HOME", dir.join(".git/dffy-test-cache"));
        cmd.env("XDG_STATE_HOME", dir.join(".git/dffy-test-state"));
        cmd.env("XDG_RUNTIME_DIR", runtime);
        cmd.env("DFFY_DAEMON_IDLE_SECS", "1");
        cmd.env("PATH", common::path_without_review_plugins());
        let child = pair.slave.spawn_command(cmd).expect("spawn the viewer");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("reader");
        std::thread::spawn(move || std::io::copy(&mut reader, &mut std::io::sink()));
        let start = std::time::Instant::now();
        loop {
            let out = Command::new(env!("CARGO_BIN_EXE_dffy"))
                .args(["ctl", "views"])
                .env("XDG_RUNTIME_DIR", runtime)
                .output()
                .expect("dffy ctl views");
            if String::from_utf8_lossy(&out.stdout).starts_with("view ") {
                break;
            }
            assert!(
                start.elapsed() < std::time::Duration::from_secs(10),
                "the viewer never registered"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Self {
            _master: pair.master,
            child,
        }
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// The viewer tools say so when no viewer runs in the repository, and
/// drive the one that does.
#[test]
fn the_viewer_tools_drive_the_viewer_running_in_the_repository() {
    let repo = repo();
    let runtime = tempfile::tempdir().expect("tempdir");
    let mut server = Server::start_in(repo.path(), runtime.path());
    initialize(&mut server);
    for (tool, arguments) in [
        ("open_in_viewer", r#"{"path":"lib.rs","line":1}"#),
        ("focus_thread", r#"{"id":"0123"}"#),
        ("viewer_selection", "{}"),
    ] {
        let call = server.call(tool, arguments);
        assert!(call.is_error(), "{tool}: {}", call.line);
        assert!(
            call.text().starts_with("No dffy viewer is running in "),
            "{tool}: {}",
            call.text()
        );
    }

    let _viewer = Viewer::start(repo.path(), runtime.path(), &["main~1", "main"]);
    let selection = server.call("viewer_selection", "{}");
    assert_eq!(selection.structured().get("selection"), Some(&Value::Null));
    assert!(
        selection
            .text()
            .starts_with("Nothing is selected in the viewer"),
        "{}",
        selection.text()
    );
    let opened = server.call("open_in_viewer", r#"{"path":"long.txt","line":30}"#);
    let at = opened.structured();
    assert_eq!(at.get("path").and_then(Value::as_str), Some("long.txt"));
    assert_eq!(at.get("line").and_then(Value::as_usize), Some(30));
    assert_eq!(opened.text(), "The viewer shows long.txt at line 30.");
    let missing = server.call("open_in_viewer", r#"{"path":"nope.rs"}"#);
    assert!(missing.is_error());
    assert!(
        missing.text().contains("nope.rs isn't in this diff"),
        "{}",
        missing.text()
    );
    let unknown = server.call("focus_thread", r#"{"id":"0123"}"#);
    assert!(unknown.is_error());
    assert!(
        unknown.text().contains("no comment 0123"),
        "{}",
        unknown.text()
    );
    let wrong = server.call("open_in_viewer", r#"{"path":"lib.rs","side":"left"}"#);
    assert!(wrong.is_error());
    assert!(
        wrong.text().contains("side must be old or new"),
        "{}",
        wrong.text()
    );
}

/// `open_in_viewer` of a file the viewer's diff doesn't change shows it
/// whole and says so; a path with no file there, or one into `.git`, is a
/// tool error.
#[test]
fn open_in_viewer_shows_a_file_outside_the_diff() {
    let repo = repo();
    let d = repo.path();
    write(d, "long.txt", &long_file(|i| i == 3));
    let runtime = tempfile::tempdir().expect("tempdir");
    let mut server = Server::start_in(d, runtime.path());
    initialize(&mut server);
    let _viewer = Viewer::start(d, runtime.path(), &[]);
    let opened = server.call("open_in_viewer", r#"{"path":"lib.rs","line":2}"#);
    assert!(!opened.is_error(), "{}", opened.line);
    let at = opened.structured();
    assert_eq!(at.get("line").and_then(Value::as_usize), Some(2));
    assert_eq!(at.get("listed"), Some(&Value::Bool(false)));
    assert_eq!(
        opened.text(),
        "The viewer shows lib.rs at line 2. It isn't in the diff, so it shows whole at the new side."
    );
    for path in ["nope.rs", ".git/HEAD"] {
        let missing = server.call("open_in_viewer", &format!(r#"{{"path":"{path}"}}"#));
        assert!(missing.is_error(), "{path}");
        assert!(
            missing
                .text()
                .contains(&format!("{path} isn't in this diff, nor at its new side")),
            "{}",
            missing.text()
        );
    }
}

/// A comments file with thread `s1` on new line 10 of long.txt, whose
/// suggestion is `line ten`.
fn suggested(d: &Path) {
    write(
        d,
        ".git/dffy/comments.json",
        r#"{"version":1,"comments":[
{"id":"s1","path":"long.txt","side":"new","start":{"old":10,"new":10},"end":{"old":10,"new":10},"text":"Spell it out.","suggestion":"line ten","author":"","created":"2026-11-17T10:00:00Z","resolved":false,"anchor":{"before":["line 8","line 9"],"lines":["line 10"],"after":["line 11","line 12"]}}
]}
"#,
    );
}

/// `reply_to_thread` writes a reply by the client, never the user's, with
/// its suggestion; an id no comment has is a tool error.
#[test]
fn reply_to_thread_writes_a_reply_by_the_client() {
    let repo = repo();
    let d = repo.path();
    suggested(d);
    let mut server = Server::start(d);
    initialize(&mut server);
    let reply = server.call(
        "reply_to_thread",
        r#"{"id":"s1","text":"@claude Spelled out.","suggestion":"line ten"}"#,
    );
    assert!(!reply.is_error(), "{}", reply.line);
    let reply = reply.structured();
    assert_eq!(
        (
            reply.get("thread").and_then(Value::as_str),
            reply.get("author").and_then(Value::as_str)
        ),
        (Some("s1"), Some("test"))
    );
    let file = std::fs::read_to_string(d.join(".git/dffy/comments.json")).expect("read");
    let last = file.lines().rev().nth(1).expect("the reply");
    for field in [
        r#""author":"test""#,
        r#""in_reply_to":"s1""#,
        r#""suggestion":"line ten""#,
    ] {
        assert!(last.contains(field), "{field} in {last}");
    }
    assert!(!last.contains("local_only"), "{last}");
    let missing = server.call("reply_to_thread", r#"{"id":"nope","text":"Hm."}"#);
    assert!(missing.is_error(), "{}", missing.line);
}

/// The client's name loses its control characters, and one that would
/// pass for the user's own comments is `dffy mcp`.
#[test]
fn a_reply_is_by_the_clients_printable_name_never_yours() {
    let repo = repo();
    let d = repo.path();
    suggested(d);
    for (name, author) in [
        (r"my\u001b[31m\nbot", "my[31mbot"),
        (" You ", "dffy mcp"),
        (r"\u0007", "dffy mcp"),
    ] {
        let mut server = Server::start(d);
        let info = format!(
            r#""protocolVersion":"2025-11-25","capabilities":{{}},"clientInfo":{{"name":"{name}","version":"1"}}"#
        );
        let (line, value) = server.request("initialize", &info);
        assert!(value.get("result").is_some(), "{line}");
        let reply = server.call("reply_to_thread", r#"{"id":"s1","text":"Done."}"#);
        let structured = reply.structured();
        assert_eq!(
            structured.get("author").and_then(Value::as_str),
            Some(author),
            "{name}"
        );
    }
}

/// `apply_suggestion` has the viewer put a thread's suggestion in the
/// working tree, saying where. The thread moves to the lines it put there,
/// placed rather than lost, and its suggestion is marked applied, which a
/// second apply refuses.
#[test]
fn apply_suggestion_writes_the_working_tree_through_the_viewer() {
    let repo = repo();
    let d = repo.path();
    write(d, "long.txt", &long_file(|i| i == 3));
    suggested(d);
    let runtime = tempfile::tempdir().expect("tempdir");
    let mut server = Server::start_in(d, runtime.path());
    initialize(&mut server);
    let _viewer = Viewer::start(d, runtime.path(), &[]);
    let opened = server.call("open_in_viewer", r#"{"path":"long.txt"}"#);
    assert!(!opened.is_error(), "{}", opened.line);
    let applied = server.call("apply_suggestion", r#"{"id":"s1"}"#);
    assert!(!applied.is_error(), "{}", applied.line);
    let at = applied.structured();
    let number = |key: &str| at.get(key).and_then(Value::as_usize);
    assert_eq!(
        (number("start"), number("end"), number("lines")),
        (Some(10), Some(10), Some(1))
    );
    assert_eq!(
        applied.text(),
        "Applied comment s1's suggestion to long.txt:10: an unstaged change."
    );
    let text = std::fs::read_to_string(d.join("long.txt")).expect("read");
    assert_eq!(
        text,
        long_file(|i| i == 3).replace("line 10\n", "line ten\n")
    );
    let listed = server.call("list_comments", r#"{"placed":true}"#);
    let listed = listed.structured();
    let s1 = (listed.get("comments").and_then(Value::as_array))
        .and_then(|c| c.first())
        .expect("s1");
    assert!(s1.get("applied").and_then(Value::as_str).is_some());
    assert_eq!(
        (s1.get("anchor").and_then(|a| a.get("lines")))
            .and_then(Value::as_array)
            .and_then(|l| l.first())
            .and_then(Value::as_str),
        Some("line ten")
    );
    let placed = (listed.get("placed").and_then(|p| p.get("s1"))).expect("placed");
    let number = |key: &str| placed.get(key).and_then(Value::as_usize);
    assert_eq!(
        (number("start"), number("end"), placed.get("lost")),
        (Some(10), Some(10), Some(&Value::Bool(false)))
    );
    let again = server.call("apply_suggestion", r#"{"id":"s1"}"#);
    assert!(again.is_error(), "{}", again.line);
    assert!(again.text().contains("applied already"), "{}", again.text());
}

/// `dffy ctl apply-suggestion` does what the MCP tool does, for an agent
/// with a shell: the viewer applies the suggestion, and refuses it again.
#[test]
fn ctl_apply_suggestion_writes_the_working_tree_through_the_viewer() {
    let repo = repo();
    let d = repo.path();
    write(d, "long.txt", &long_file(|i| i == 3));
    suggested(d);
    let runtime = tempfile::tempdir().expect("tempdir");
    let _viewer = Viewer::start(d, runtime.path(), &[]);
    let ctl = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_dffy"))
            .arg("ctl")
            .args(args)
            .current_dir(d)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("XDG_CONFIG_HOME", d.join(".no-config"))
            .output()
            .expect("dffy ctl");
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        (out.status.code(), text(&out.stdout), text(&out.stderr))
    };
    let (code, _, said) = ctl(&["open", "long.txt"]);
    assert_eq!(code, Some(0), "{said}");
    let (code, out, said) = ctl(&["apply-suggestion", "s1"]);
    assert_eq!(code, Some(0), "{said}");
    assert!(
        out.ends_with(" applied comment s1's suggestion to long.txt:10: an unstaged change\n"),
        "{out}"
    );
    let text = std::fs::read_to_string(d.join("long.txt")).expect("read");
    assert_eq!(
        text,
        long_file(|i| i == 3).replace("line 10\n", "line ten\n")
    );
    let (code, _, said) = ctl(&["apply-suggestion", "s1"]);
    assert_eq!(code, Some(128));
    assert!(said.contains("applied already"), "{said}");
}

/// A C repository: `include/slab.h` defines `kmalloc_array`, which the
/// second commit changes, `mm/use.c` calls it twice, and
/// `tools/include/slab.h` defines another.
fn definitions_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    let slab = |body: &str| {
        format!(
            "#ifndef SLAB_H\n#define SLAB_H\n\n\
             static inline void *kmalloc_array(unsigned long n, unsigned long size)\n\
             {{\n\treturn {body};\n}}\n\n#endif\n"
        )
    };
    write(d, "include/slab.h", &slab("alloc(n * size)"));
    write(
        d,
        "tools/include/slab.h",
        "static inline void *kmalloc_array(unsigned long n, unsigned long size)\n{\n\treturn 0;\n}\n",
    );
    let user = |size: &str| {
        format!(
            "#include <slab.h>\n\nint use(void)\n{{\n\tvoid *p = kmalloc_array(4, {size});\n\
             \tvoid *q = kmalloc_array(2, 2);\n\treturn p != q;\n}}\n"
        )
    };
    write(d, "mm/use.c", &user("8"));
    write(d, "README.md", "# notes\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "first"]);
    write(d, "include/slab.h", &slab("alloc_checked(n, size)"));
    write(d, "mm/use.c", &user("16"));
    git(d, &["commit", "-q", "-am", "second"]);
    dir
}

/// `definitions` looks a name up as the viewer's peek does, without a
/// viewer: nearest first, saying whether this diff changes each and how
/// far the index is built, which the first calls may not have whole. A
/// name with no definition gets its text matches; `references` pages the
/// lines using it.
#[test]
fn definitions_look_names_up_without_a_viewer() {
    let repo = definitions_repo();
    let d = repo.path();
    let mut server = Server::start(d);
    initialize(&mut server);
    let ask = r#"{"name":"kmalloc_array","path":"mm/use.c","range":"HEAD~1..HEAD"}"#;
    let mut call = server.call("definitions", ask);
    for _ in 0..200 {
        if call.structured().get("partial") == Some(&Value::Bool(false)) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        call = server.call("definitions", ask);
    }
    let found = call.structured();
    assert_eq!(
        found.get("partial"),
        Some(&Value::Bool(false)),
        "{}",
        call.line
    );
    assert_eq!(found.get("total").and_then(Value::as_usize), Some(2));
    let first = &found
        .get("candidates")
        .and_then(Value::as_array)
        .expect("candidates")[0];
    let field = |v: &Value, k: &str| v.get(k).cloned().unwrap_or(Value::Null);
    assert_eq!(field(first, "path").as_str(), Some("include/slab.h"));
    assert_eq!(field(first, "start").as_usize(), Some(4));
    assert_eq!(field(first, "kind").as_str(), Some("function"));
    assert_eq!(field(first, "changed"), Value::Bool(true));
    assert_eq!(field(first, "in_hunk"), Value::Bool(true));
    assert_eq!(field(first, "text_match"), Value::Bool(false));
    assert!(call.text().contains("include/slab.h:4"), "{}", call.text());

    let old = server.call(
        "definitions",
        r#"{"name":"kmalloc_array","path":"mm/use.c","range":"HEAD~1..HEAD","side":"old"}"#,
    );
    assert_eq!(
        old.structured().get("side").and_then(Value::as_str),
        Some("old")
    );

    let text = server.call(
        "definitions",
        r#"{"name":"alloc_checked","path":"mm/use.c","range":"HEAD~1..HEAD"}"#,
    );
    let first = &text
        .structured()
        .get("candidates")
        .and_then(Value::as_array)
        .expect("candidates")[0];
    assert_eq!(
        field(first, "text_match"),
        Value::Bool(true),
        "{}",
        text.line
    );
    assert_eq!(field(first, "path").as_str(), Some("include/slab.h"));

    let page = |cursor: Option<&str>| {
        let cursor = cursor.map_or(String::new(), |c| format!(r#","cursor":"{c}""#));
        format!(
            r#"{{"name":"kmalloc_array","path":"mm/use.c","range":"HEAD~1..HEAD","references":true,"limit":1{cursor}}}"#
        )
    };
    let refs = server.call("definitions", &page(None));
    let uses = |call: &Call| -> Vec<String> {
        let refs = call.structured().get("references").expect("references");
        (refs.get("matches").and_then(Value::as_array))
            .expect("matches")
            .iter()
            .map(|m| {
                format!(
                    "{}:{}",
                    field(m, "path").as_str().unwrap_or(""),
                    field(m, "line").as_usize().unwrap_or(0)
                )
            })
            .collect()
    };
    assert_eq!(uses(&refs), ["mm/use.c:5"]);
    let refs_of =
        |call: &Call, k: &str| field(call.structured().get("references").expect("references"), k);
    let counts = |call: &Call| ["files", "total", "complete"].map(|k| refs_of(call, k));
    let [one, two] = [1.0, 2.0].map(Value::Number);
    // The search stops once the page is full.
    assert_eq!(
        counts(&refs),
        [one.clone(), two.clone(), Value::Bool(false)]
    );
    let next = refs.cursor().expect("a next page").to_owned();
    let rest = server.call("definitions", &page(Some(&next)));
    assert_eq!(
        (uses(&rest), rest.cursor()),
        (vec!["mm/use.c:6".to_owned()], None)
    );
    assert_eq!(counts(&rest), [one, two, Value::Bool(true)]);
}

/// Asked from a file of a language without definitions, `definitions`
/// says so.
#[test]
fn definitions_refuse_a_file_of_another_language() {
    let repo = definitions_repo();
    let mut server = Server::start(repo.path());
    initialize(&mut server);
    let readme = server.call("definitions", r#"{"name":"notes","path":"README.md"}"#);
    assert!(readme.is_error());
    assert!(
        readme.text().contains("doesn't read README.md's language"),
        "{}",
        readme.text()
    );
}

/// A working-tree file the diff already modifies, edited again between two
/// calls, is read again: the second call's lines are the file's new ones.
#[test]
fn definitions_read_a_file_edited_again_between_calls() {
    let repo = definitions_repo();
    let d = repo.path();
    let defines = "static inline void *kmalloc_array(unsigned long n, unsigned long size)\n{\n\treturn 0;\n}\n";
    write(d, "include/slab.h", &format!("/* one */\n{defines}"));
    let mut server = Server::start(d);
    initialize(&mut server);
    let ask = r#"{"name":"kmalloc_array","path":"mm/use.c"}"#;
    let start = |server: &mut Server| {
        let call = server.call("definitions", ask);
        let found = call.structured();
        let first = &found
            .get("candidates")
            .and_then(Value::as_array)
            .expect("candidates")[0];
        assert_eq!(
            first.get("path").and_then(Value::as_str),
            Some("include/slab.h"),
            "{}",
            call.line
        );
        first.get("start").and_then(Value::as_usize)
    };
    assert_eq!(start(&mut server), Some(2));
    write(
        d,
        "include/slab.h",
        &format!("/* one */\n/* two */\n/* three */\n{defines}"),
    );
    assert_eq!(start(&mut server), Some(4), "the file's lines as edited");
}

//! The built binary end to end, as dffy runs it: a session over stdin and
//! stdout, JSON-RPC a line, its stdout kept in a file. A real git
//! repository whose `origin` is a local bare repository holding the PR's
//! ref, a stub `gh` on PATH that replays fixtures in GitHub's GraphQL shape
//! and logs each request, and the dffy built with the workspace for `dffy
//! comments`.
//!
//! Most tests ask one request a session through [`Fixture::run`], which
//! reads what the request said back as the lines [`said`] makes of it.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

const PR: &str = "7";

const URL: &str = "https://github.com/acme/widgets/pull/7";

const ORIGIN: &str = "git@github.com:acme/widgets.git";

/// The dffy built with the workspace, when its `comments apply` works.
fn real_dffy() -> Option<PathBuf> {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_dffy-review-github"));
    let dffy = exe.parent()?.join("dffy");
    let dir = tempfile::tempdir().ok()?;
    let mut child = Command::new(&dffy)
        .arg("--comments")
        .arg(dir.path().join("probe.json"))
        .args(["comments", "apply"])
        .env("XDG_CONFIG_HOME", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(b"[]").ok()?;
    child.wait().ok()?.success().then_some(dffy)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada Lovelace")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .trim()
        .to_owned()
}

/// The git on the test's own PATH.
fn real_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("git"))
        .find(|git| git.is_file())
        .expect("git on PATH")
}

fn numbered(lines: impl IntoIterator<Item = String>) -> String {
    lines.into_iter().map(|l| l + "\n").collect()
}

const WAIT: Duration = Duration::from_secs(30);

/// A session with the plugin, its stdout going to `out`.
struct Session {
    child: Child,
    stdin: ChildStdin,
    out: PathBuf,
    /// Lines of `out` read so far.
    read: usize,
    /// The dffy that writes what the plugin says to write, and the
    /// comments file it goes to, as dffy keeps one.
    writer: Option<(PathBuf, PathBuf)>,
}

/// A comments file as dffy reads it: what it holds, `null` when there's
/// none yet.
fn carried(file: &Path) -> Value {
    fs::read(file).map_or(Value::Null, |bytes| {
        serde_json::from_slice(&bytes).expect("a comments file")
    })
}

/// What dffy carries of a review's comments file in each request about
/// the review, as docs/plugins.md has it, the way these tests make dffy's
/// requests. [`the_harness_carries_what_dffy_carries`] holds it to dffy's
/// own requests for the same file.
mod carry {
    use serde_json::{Map, Value, json};

    /// The fields of a comment dffy knows; the others are the
    /// integration's.
    const DFFY: [&str; 25] = [
        "id",
        "path",
        "old_path",
        "side",
        "start_side",
        "start",
        "end",
        "text",
        "suggestion",
        "author",
        "created",
        "in_reply_to",
        "resolved",
        "ref",
        "anchor",
        "distance",
        "url",
        "outdated",
        "kind",
        "label",
        "branch",
        "pending",
        "post_error",
        "host_resolved",
        "moved_from",
    ];

    fn comments(file: &Value) -> &[Value] {
        file["comments"].as_array().map_or(&[], Vec::as_slice)
    }

    fn integration(c: &Value) -> impl Iterator<Item = (&String, &Value)> {
        c.as_object()
            .into_iter()
            .flatten()
            .filter(|(k, _)| !DFFY.contains(&k.as_str()))
    }

    fn unsent(c: &Value) -> bool {
        c["ref"].is_string()
            && c["in_reply_to"].is_null()
            && c["host_resolved"].is_boolean()
            && c["host_resolved"] != c["resolved"].as_bool().unwrap_or_default()
    }

    /// `prepare`'s and `pull`'s `known`.
    pub(crate) fn known(file: &Value) -> Value {
        let known = comments(file).iter().filter(|c| c["ref"].is_string());
        Value::Array(
            known
                .map(|c| {
                    let mut entry = json!({"ref": c["ref"]});
                    if c["anchor"].is_object() {
                        entry["anchored"] = json!(true);
                    }
                    if c["pending"] == true {
                        entry["pending"] = json!(true);
                    }
                    for (k, v) in integration(c) {
                        entry[k] = v.clone();
                    }
                    entry
                })
                .collect(),
        )
    }

    /// `dryRun`'s and `post`'s `drafts`: a reply only in a thread not kept
    /// local.
    pub(crate) fn drafts(file: &Value) -> Vec<Value> {
        let all = comments(file);
        let drafts = all.iter().filter(|c| {
            let root =
                (c["in_reply_to"].as_str()).and_then(|id| all.iter().find(|r| r["id"] == id));
            c["ref"].is_null()
                && c["author"].as_str().unwrap_or_default().is_empty()
                && c["local_only"] != true
                && root.is_none_or(|r| r["local_only"] != true)
        });
        drafts
            .map(|c| {
                let mut draft = Map::new();
                for key in [
                    "id",
                    "path",
                    "old_path",
                    "start_side",
                    "start",
                    "end",
                    "text",
                    "suggestion",
                    "anchor",
                    "distance",
                ] {
                    if !c[key].is_null() {
                        draft.insert(key.to_owned(), c[key].clone());
                    }
                }
                if !c["end"].is_null() {
                    draft.insert(
                        "side".to_owned(),
                        json!(c["side"].as_str().unwrap_or("new")),
                    );
                }
                if let Some(root) = c["in_reply_to"].as_str() {
                    let root = all.iter().find(|r| r["id"] == root);
                    let to = match root.filter(|r| r["ref"].is_string()) {
                        Some(root) => {
                            let mut to = json!({"ref": root["ref"]});
                            for (k, v) in integration(root) {
                                to[k] = v.clone();
                            }
                            to
                        }
                        None => json!({"id": c["in_reply_to"]}),
                    };
                    draft.insert("in_reply_to".to_owned(), to);
                } else if c["resolved"] == true {
                    draft.insert("resolved".to_owned(), json!(true));
                }
                Value::Object(draft)
            })
            .collect()
    }

    /// `dryRun`'s and `post`'s `resolves`.
    pub(crate) fn resolves(file: &Value) -> Vec<Value> {
        comments(file)
            .iter()
            .filter(|c| unsent(c))
            .map(|c| {
                let mut entry =
                    json!({"ref": c["ref"], "resolved": c["resolved"], "path": c["path"]});
                if !c["end"].is_null() {
                    entry["side"] = json!(c["side"].as_str().unwrap_or("new"));
                    entry["end"] = c["end"].clone();
                }
                for (k, v) in integration(c) {
                    entry[k] = v.clone();
                }
                entry
            })
            .collect()
    }
}

/// The harness speaks for dffy, so it must carry what dffy carries: for
/// the comments file in dffy's protocol fixtures, each request is the one
/// dffy's own test holds dffy's requests to.
#[test]
fn the_harness_carries_what_dffy_carries() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/dffy/tests/fixtures/review-protocol");
    let read = |name: &str| carried(&dir.join(name));
    let (file, local, expected) = (
        read("review.json"),
        read("local.json"),
        read("requests.json"),
    );
    assert_eq!(
        json!({"id": "7", "known": carry::known(&file)}),
        expected["prepare"]
    );
    let mut drafts = carry::drafts(&file);
    assert_eq!(
        json!({"id": "7", "event": "approve", "labels": {"Code-Review": -1}, "summary": "Ship it",
            "drafts": drafts, "resolves": carry::resolves(&file)}),
        expected["post"]
    );
    drafts.extend(carry::drafts(&local));
    assert_eq!(
        json!({"id": "7", "drafts": drafts, "resolves": carry::resolves(&file)}),
        expected["dryRun"]
    );
}

impl Session {
    /// Starts `command` and initializes it.
    fn start(command: &mut Command, out: PathBuf) -> Self {
        let file = fs::File::create(&out).expect("stdout file");
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(file)
            .spawn()
            .expect("runs");
        let stdin = child.stdin.take().expect("stdin");
        let mut session = Self {
            child,
            stdin,
            out,
            read: 0,
            writer: None,
        };
        let (_, init) = session.call(
            0,
            "initialize",
            &json!({"protocol": 1, "client": {"name": "test"}}),
        );
        let init = init.expect("initialized");
        assert_eq!(init["protocol"], 1);
        session
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("sent");
    }

    /// The next line the plugin wrote but what it says to write, which is
    /// written, as dffy does.
    fn said(&mut self) -> Value {
        loop {
            let message = self.next();
            if message["method"] != "comments/apply" {
                return message;
            }
            self.write(&message["params"]["ops"]);
        }
    }

    /// Writes `ops` to the comments file with `dffy comments apply`.
    fn write(&self, ops: &Value) {
        let (dffy, file) = self.writer.as_ref().expect("a comments file to write");
        let mut child = Command::new(dffy)
            .arg("--comments")
            .arg(file)
            .args(["comments", "apply"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("dffy runs");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(ops.to_string().as_bytes())
            .expect("ops written");
        assert!(child.wait().expect("dffy exits").success(), "{ops}");
    }

    fn notify(&mut self, method: &str, params: &Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// The next line the plugin wrote.
    fn next(&mut self) -> Value {
        let start = Instant::now();
        loop {
            let text = fs::read_to_string(&self.out).expect("stdout");
            let mut lines = text.lines().skip(self.read);
            if let Some(line) = lines
                .next()
                .filter(|_| text.ends_with('\n') || lines.next().is_some())
            {
                self.read += 1;
                return serde_json::from_str(line).expect("a message is JSON");
            }
            assert!(start.elapsed() < WAIT, "no answer: {text}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Asks `method`, and returns the notifications that came before its
    /// answer, then the answer: its result, or its error. What it says to
    /// write is written as it comes, as dffy writes it, and isn't returned.
    fn call(
        &mut self,
        id: u64,
        method: &str,
        params: &Value,
    ) -> (Vec<Value>, Result<Value, Value>) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let mut notes = Vec::new();
        loop {
            let message = self.said();
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

    /// Shuts the plugin down: it answers once what it runs is done, and
    /// exits.
    fn close(mut self) {
        let (_, done) = self.call(u64::MAX, "shutdown", &Value::Null);
        assert_eq!(done, Ok(Value::Null));
        self.notify("exit", &Value::Null);
        assert!(self.child.wait().expect("exits").success());
    }
}

/// What a request said, as lines: each notification, then its result's
/// parts, then its error as `{"error": …}`. A session or progress
/// notification is `{"session": …}` or `{"progress": …}`, a moved head
/// `{"head": …, "moved": true}`; a dry run's result is a `{"draft": …}`
/// for each draft, a post's a draft for each left unposted then its
/// counts; others are their result, `{}` none.
fn said(method: &str, notes: Vec<Value>, answer: Result<Value, Value>) -> Run {
    let mut lines: Vec<Value> = notes
        .into_iter()
        .map(|note| {
            let params = &note["params"];
            match note["method"].as_str() {
                Some("session") => json!({"session": params["session"]}),
                Some("progress") => json!({"progress": params["progress"]}),
                Some("head/moved") => json!({"head": params["head"], "moved": true}),
                _ => note,
            }
        })
        .collect();
    let mut result = |result: Value| match method {
        "dryRun" => {
            assert_eq!(
                result.as_object().map(serde_json::Map::len),
                Some(1),
                "drafts alone: {result}"
            );
            let drafts = result["drafts"].as_array().into_iter().flatten();
            lines.extend(drafts.map(|d| json!({"draft": d})));
        }
        "post" => {
            let mut result = result;
            let unposted = result["unposted"].take();
            lines.extend(
                unposted
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|d| json!({"draft": d})),
            );
            result
                .as_object_mut()
                .expect("an object")
                .remove("unposted");
            lines.push(result);
        }
        "prepare" | "pull" => assert_eq!(result, json!({})),
        _ => lines.push(result),
    };
    let code = match answer {
        Ok(answer) => {
            result(answer);
            0
        }
        Err(error) => {
            if let Some(data) = error.get("data") {
                result(data.clone());
            }
            lines.push(json!({"error": error["message"]}));
            1
        }
    };
    Run {
        code: Some(code),
        lines,
    }
}

/// The request dffy sends for what the one-shot verbs were: `prepare 7`,
/// `post 7 --dry-run [--drafts <file>]`, `post 7 --event <event>
/// --summary-file <file>`, `prune [--days <n>]`.
fn request(args: &[&str]) -> (&'static str, Value) {
    match args {
        ["doctor"] => ("doctor", json!({})),
        ["list"] => ("list", json!({})),
        ["detect"] => ("detect", json!({})),
        ["prune"] => ("prune", json!({})),
        ["prune", "--days", days] => ("prune", json!({"days": days.parse::<u64>().expect("days")})),
        ["prepare", pr] => ("prepare", json!({"id": pr})),
        ["pull", pr] => ("pull", json!({"id": pr})),
        ["post", pr, "--dry-run"] => ("dryRun", json!({"id": pr})),
        ["post", pr, "--dry-run", "--drafts", file] => (
            "dryRun",
            json!({"id": pr, "drafts": carry::drafts(&carried(Path::new(file)))}),
        ),
        ["post", pr, "--event", event, "--summary-file", file] => (
            "post",
            json!({"id": pr, "event": event,
                "summary": fs::read_to_string(file).expect("the summary")}),
        ),
        other => panic!("no request for {other:?}"),
    }
}

struct Run {
    code: Option<i32>,
    lines: Vec<Value>,
}

/// A session watching the PR, which a test makes ask GitHub now.
struct Watching {
    session: Session,
    /// GitHub's REST requests logged so far.
    asked: usize,
    rest: PathBuf,
}

impl Watching {
    /// Back from idle: the plugin asks GitHub about the PR at once. Returns
    /// once it has, with what it pushed for the look before.
    fn look(&mut self) {
        self.session.notify("idle/changed", &json!({"idle": true}));
        self.session.notify("idle/changed", &json!({"idle": false}));
        self.asked += 1;
        let start = Instant::now();
        while fs::read_to_string(&self.rest)
            .unwrap_or_default()
            .lines()
            .count()
            < self.asked
        {
            assert!(start.elapsed() < WAIT, "it didn't ask");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The next thing pushed.
    fn pushed(&mut self) -> Value {
        let push = self.session.next();
        json!({"method": push["method"], "params": push["params"]})
    }

    /// What it pushed until it asked GitHub again: nothing, once the look
    /// before is done.
    fn settled(mut self) -> Vec<Value> {
        self.look();
        let mut pushed = Vec::new();
        let text = fs::read_to_string(&self.session.out).expect("stdout");
        let unread = text.lines().count() - self.session.read;
        for _ in 0..unread {
            pushed.push(self.pushed());
        }
        self.session.close();
        pushed
    }
}

impl Run {
    fn ok(&self) -> &Self {
        assert_eq!(self.code, Some(0), "{:?}", self.lines);
        self
    }

    fn session(&self) -> &Value {
        &self.lines[0]["session"]
    }

    fn last(&self) -> &Value {
        self.lines.last().expect("a line")
    }

    fn error(&self) -> &str {
        self.last()["error"]
            .as_str()
            .unwrap_or_else(|| panic!("no error line: {:?}", self.lines))
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    real_dffy: PathBuf,
    base: String,
    head: String,
    /// A newer head, pushed to the PR's ref by `move_head`.
    head2: String,
}

impl Fixture {
    /// Panics when the workspace's dffy can't apply comments: every verb
    /// writes through it, and a skipped test would pass silently.
    fn new() -> Self {
        let real_dffy =
            real_dffy().expect("dffy comments apply isn't built; cargo build -p dffy builds it");
        let dir = tempfile::tempdir().expect("temp dir");
        let d = dir.path();
        let remote = d.join("github/acme/widgets.git");
        fs::create_dir_all(&remote).expect("remote dir");
        git(&remote, &["init", "-q", "--bare", "-b", "main"]);

        let author = d.join("author");
        fs::create_dir_all(author.join("src")).expect("author dir");
        git(&author, &["init", "-q", "-b", "main"]);
        let base_lib = numbered((1..=12).map(|n| format!("line {n}")));
        fs::write(author.join("src/lib.rs"), &base_lib).expect("write");
        fs::write(author.join("notes.txt"), "notes\n").expect("write");
        let long = |n: usize, changed: &str| {
            numbered((1..=40).map(|i| {
                if i == n {
                    changed.to_owned()
                } else {
                    format!("long {i}")
                }
            }))
        };
        let moved = |changed: &str| {
            numbered((1..=20).map(|i| {
                if i == 10 {
                    changed.to_owned()
                } else {
                    format!("moved {i}")
                }
            }))
        };
        fs::write(author.join("src/long.rs"), long(0, "")).expect("write");
        fs::create_dir_all(author.join("docs")).expect("docs dir");
        fs::write(author.join("docs/old name.txt"), moved("moved 10")).expect("write");
        git(&author, &["add", "."]);
        git(&author, &["commit", "-q", "-m", "base"]);
        let base = git(&author, &["rev-parse", "HEAD"]);
        // Two lines on top, so its old and new line numbers differ.
        fs::write(
            author.join("src/long.rs"),
            format!("zero\nzero\n{}", long(30, "thirty")),
        )
        .expect("write");
        git(&author, &["mv", "docs/old name.txt", "docs/new name.txt"]);
        fs::write(author.join("docs/new name.txt"), moved("ten")).expect("write");
        let head_lib = numbered(
            (1..=12)
                .map(|n| {
                    if n == 5 {
                        "five".to_owned()
                    } else {
                        format!("line {n}")
                    }
                })
                .chain(["thirteen".to_owned()]),
        );
        fs::write(author.join("src/lib.rs"), &head_lib).expect("write");
        git(&author, &["commit", "-q", "-am", "head"]);
        let head = git(&author, &["rev-parse", "HEAD"]);
        fs::write(author.join("notes.txt"), "notes\nmore\n").expect("write");
        git(&author, &["commit", "-q", "-am", "head 2"]);
        let head2 = git(&author, &["rev-parse", "HEAD"]);
        let url = remote.to_str().expect("utf-8 path");
        git(
            &author,
            &["push", "-q", url, &format!("{base}:refs/heads/main")],
        );
        git(
            &author,
            &["push", "-q", url, &format!("{head}:refs/pull/{PR}/head")],
        );
        // In the remote for `move_head`, under a ref a clone doesn't copy.
        git(
            &author,
            &["push", "-q", url, &format!("{head2}:refs/hidden/next")],
        );

        git(d, &["clone", "-q", url, "work"]);
        // On GitHub as far as the plugin can tell; the stub ssh fetches
        // from the bare repository.
        git(&d.join("work"), &["remote", "set-url", "origin", ORIGIN]);
        fs::create_dir_all(d.join("stub/bin")).expect("stub dir");
        fs::create_dir_all(d.join("stub/gh")).expect("stub dir");
        for (path, script) in [
            ("stub/bin/gh", include_str!("fixtures/stubs/gh.sh")),
            ("stub/ssh", include_str!("fixtures/stubs/ssh.sh")),
        ] {
            fs::write(d.join(path), script).expect("stub");
            fs::set_permissions(d.join(path), fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        std::os::unix::fs::symlink(&real_dffy, d.join("stub/bin/dffy")).expect("dffy on PATH");
        Self {
            dir,
            real_dffy,
            base,
            head,
            head2,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn work(&self) -> PathBuf {
        self.path("work")
    }

    /// Answers the `n`th `operation` (every one when `None`) with a fixture.
    fn reply(&self, operation: &str, n: Option<usize>, fixture: &str) {
        let source = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(fixture),
        )
        .expect("fixture");
        let text = source
            .replace("@BASE@", &self.base)
            .replace("@HEAD2@", &self.head2)
            .replace("@HEAD@", &self.head);
        let suffix = if fixture.ends_with("rejected.json") {
            ".error.json"
        } else {
            ".json"
        };
        let name = match n {
            Some(n) => format!("{operation}.{n}{suffix}"),
            None => format!("{operation}{suffix}"),
        };
        fs::write(self.path("stub/gh").join(name), text).expect("fixture written");
    }

    /// Makes the `n`th `operation` (every one when `None`) fail the way an
    /// unreachable API does.
    fn down(&self, operation: &str, n: Option<usize>) {
        let name = match n {
            Some(n) => format!("{operation}.{n}.down"),
            None => format!("{operation}.down"),
        };
        fs::write(self.path("stub/gh").join(name), "").expect("write");
    }

    /// Answers the `n`th REST `operation` (every one when `None`) as `gh
    /// api --include` prints a response: its status line, `headers` and
    /// `body`, and failing past a 2xx as gh does.
    fn http(
        &self,
        operation: &str,
        n: Option<usize>,
        status: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) {
        let name = match n {
            Some(n) => format!("{operation}.{n}.http"),
            None => format!("{operation}.http"),
        };
        let headers: String = headers
            .iter()
            .map(|(k, v)| [k, ": ", v, "\r\n"].concat())
            .collect();
        fs::write(
            self.path("stub/gh").join(name),
            format!("HTTP/2.0 {status}\n{headers}\r\n{body}"),
        )
        .expect("an answer written");
    }

    /// Answers the `n`th `check` (every one when `None`) with the PR on
    /// `head`, last changed at `updated_at`, under `etag`.
    fn pull_answer(&self, n: Option<usize>, etag: &str, head: &str, updated_at: &str) {
        let body = json!({"number": 7, "head": {"sha": head}, "updated_at": updated_at});
        self.http("pull", n, "200 OK", &[("Etag", etag)], &body.to_string());
    }

    fn up(&self, operation: &str) {
        fs::remove_file(self.path("stub/gh").join(format!("{operation}.down"))).expect("remove");
    }

    fn move_head(&self) {
        let url = self.path("github/acme/widgets.git");
        git(
            &url,
            &["update-ref", &format!("refs/pull/{PR}/head"), &self.head2],
        );
    }

    /// Makes `gh` answer as it does without a token.
    fn log_out(&self) {
        fs::write(self.path("stub/gh/logged-out"), "").expect("write");
    }

    /// A request as dffy makes it, in a session of its own, from the
    /// verb's words (see [`request`]), with stdout written to a file the
    /// `gh` stub counts lines in; `prefetch`, yours to run, runs as you
    /// would.
    fn run(&self, args: &[&str]) -> Run {
        self.finish(&mut self.command(&self.stub_path()), args)
    }

    fn stub_path(&self) -> String {
        let path = std::env::var("PATH").unwrap_or_default();
        format!("{}:{path}", self.path("stub/bin").display())
    }

    /// A request made with nothing on PATH but git and dffy: a `gh`
    /// installed beside git, as on CI, would otherwise be found.
    fn run_without_gh(&self, args: &[&str]) -> Run {
        let bin = self.path("nogh");
        if !bin.exists() {
            fs::create_dir(&bin).expect("dir");
            std::os::unix::fs::symlink(real_git(), bin.join("git")).expect("git");
            std::os::unix::fs::symlink(&self.real_dffy, bin.join("dffy")).expect("dffy");
        }
        self.finish(&mut self.command(&bin.display().to_string()), args)
    }

    /// A request made with `DFFY` naming the workspace's dffy, as dffy
    /// starts the plugin.
    fn run_as_dffy(&self, args: &[&str]) -> Run {
        let mut cmd = self.command(&self.stub_path());
        self.finish(cmd.env("DFFY", &self.real_dffy), args)
    }

    fn command(&self, path: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dffy-review-github"));
        cmd.current_dir(self.work())
            .env("PATH", path)
            .env_remove("DFFY")
            .env_remove("GH_HOST")
            .env("STUB", self.path("stub"))
            .env("STUB_GIT", real_git())
            .env("STUB_REMOTES", self.path("github"))
            .env("GIT_SSH_COMMAND", self.path("stub/ssh"))
            .env("GIT_SSH_VARIANT", "simple")
            .env_remove("GH_CONFIG_DIR")
            .env("XDG_DATA_HOME", self.path("data"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        cmd
    }

    /// A session of `cmd`'s, its stdout where the `gh` stub counts it, what
    /// it says to write going to [`Fixture::comments_file`].
    fn session(&self, cmd: &mut Command) -> Session {
        let mut session = Session::start(cmd, self.path("stub/out"));
        session.writer = Some((self.real_dffy.clone(), self.comments_file()));
        session
    }

    fn finish(&self, cmd: &mut Command, args: &[&str]) -> Run {
        if args == ["prefetch"] {
            let out = cmd.args(args).stdin(Stdio::null()).output().expect("runs");
            let line = serde_json::from_slice(&out.stdout).expect("JSON");
            return Run {
                code: out.status.code(),
                lines: vec![line],
            };
        }
        let (method, mut params) = request(args);
        let file = carried(&self.comments_file());
        match method {
            "prepare" | "pull" => params["known"] = carry::known(&file),
            "dryRun" | "post" => {
                // A dry run plans the review's drafts, then the other file's.
                let mut drafts = carry::drafts(&file);
                drafts.extend(params["drafts"].as_array().into_iter().flatten().cloned());
                params["drafts"] = json!(drafts);
                params["resolves"] = json!(carry::resolves(&file));
            }
            _ => {}
        }
        let mut session = self.session(cmd);
        let (notes, answer) = session.call(1, method, &params);
        session.close();
        // As dffy does once a post with an event that submits ends well.
        if method == "post" && answer.is_ok() && params["event"] != "pending" {
            let pending: Vec<Value> = carried(&self.comments_file())["comments"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| c["pending"] == true)
                .map(|c| json!({"op": "upsert", "comment": {"ref": c["ref"], "pending": null}}))
                .collect();
            if !pending.is_empty() {
                self.apply(&json!(pending));
            }
        }
        said(method, notes, answer)
    }

    /// A session watching the PR, for [`Watching::look`].
    fn watch(&self) -> Watching {
        let mut session = self.session(&mut self.command(&self.stub_path()));
        session.notify("watch", &json!({"id": PR}));
        Watching {
            session,
            asked: self.rest().len(),
            rest: self.path("stub/rest"),
        }
    }

    /// The PR's comments file, which the test keeps as dffy does: requests
    /// carry what [`carry`] takes of it, and what the plugin says to write
    /// goes in it.
    fn comments_file(&self) -> PathBuf {
        self.path("dffy/7.json")
    }

    /// What the plugin keeps for the PR, `name` in its directory: under the
    /// data directory, one directory per repository and PR.
    fn kept(&self, name: &str) -> PathBuf {
        let sessions = self.path("data/dffy-review-github");
        let repo = fs::read_dir(&sessions)
            .expect("a session")
            .next()
            .expect("one repository")
            .expect("entry")
            .path();
        repo.join(PR).join(name)
    }

    /// The session as the plugin keeps it.
    fn saved_session(&self) -> Value {
        let path = self.kept("session.json");
        serde_json::from_slice(&fs::read(path).expect("a session")).expect("JSON")
    }

    fn save_session(&self, session: &Value) {
        let path = self.kept("session.json");
        fs::write(path, session.to_string()).expect("a session written");
    }

    /// An answer written for the stub, as JSON to change.
    fn answer(&self, name: &str) -> Value {
        let path = self.path("stub/gh").join(name);
        serde_json::from_slice(&fs::read(path).expect("an answer")).expect("JSON")
    }

    fn comments(&self) -> Value {
        let out = Command::new(&self.real_dffy)
            .current_dir(self.work())
            .arg("--comments")
            .arg(self.comments_file())
            .args(["comments", "--json"])
            .env("XDG_CONFIG_HOME", self.path("config"))
            .output()
            .expect("dffy runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("JSON")
    }

    fn comment(&self, reference: &str) -> Value {
        self.comments()["comments"]
            .as_array()
            .expect("comments")
            .iter()
            .find(|c| c["ref"] == reference)
            .cloned()
            .unwrap_or_else(|| panic!("no comment with ref {reference}"))
    }

    fn apply(&self, ops: &Value) -> Value {
        let mut child = Command::new(&self.real_dffy)
            .current_dir(self.work())
            .arg("--comments")
            .arg(self.comments_file())
            .args(["comments", "apply"])
            .env("XDG_CONFIG_HOME", self.path("config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("dffy runs");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(ops.to_string().as_bytes())
            .expect("ops written");
        let out = child.wait_with_output().expect("dffy exits");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("JSON")
    }

    /// The `gh` stub's log, `gh <operation> after <lines printed>`, in order.
    fn events(&self) -> Vec<String> {
        fs::read_to_string(self.path("stub/events"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn reset_events(&self) {
        fs::remove_file(self.path("stub/events")).expect("reset");
    }

    /// Every GraphQL request sent, in order.
    fn requests(&self) -> Vec<Value> {
        fs::read_to_string(self.path("stub/requests"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("a request is JSON"))
            .collect()
    }

    /// Every REST request, `<gh's arguments> after <lines printed>`, as
    /// answered.
    fn rest(&self) -> Vec<String> {
        fs::read_to_string(self.path("stub/rest"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn operations(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| r["operationName"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    /// A cold `prepare` of the PR with the two pages of threads, and
    /// GitHub's files with the hunks git has too, unless a test answered
    /// the files otherwise.
    fn prepare(&self) -> Run {
        self.reply("PullRequest", Some(1), "pull_request_page_1.json");
        self.reply("PullRequest", Some(2), "pull_request_page_2.json");
        self.reply("files", None, "files.json");
        let run = self.run(&["prepare", PR]);
        run.ok();
        run
    }

    fn summary(&self, text: &str) -> String {
        let file = self.path("summary.md");
        fs::write(&file, text).expect("summary written");
        file.display().to_string()
    }

    fn post(&self, event: &str, summary: &str) -> Run {
        let file = self.summary(summary);
        self.run(&["post", PR, "--event", event, "--summary-file", &file])
    }

    fn add_drafts(&self) -> Vec<String> {
        let answer = self.apply(&json!([
            {"op": "add", "comment": {"path": "src/lib.rs", "side": "new",
                "start": {"old": null, "new": 13}, "end": {"old": null, "new": 13},
                "anchor": {"before": ["line 11", "line 12"], "lines": ["thirteen"], "after": []},
                "text": "Needs a test.", "distance": 0}},
            {"op": "add", "comment": {"in_reply_to_ref": "github:PRRC_kwDOAAAAB84AAAB1", "text": "Agreed."}},
            {"op": "add", "comment": {"path": "src/lib.rs", "side": "new",
                "start": {"old": 9, "new": 9}, "end": {"old": 10, "new": 10},
                "anchor": {"before": ["line 7", "line 8"], "lines": ["line 9", "line 10"], "after": ["line 11", "line 12"]},
                "text": "Rename these.", "distance": 4}},
        ]));
        answer["ids"]
            .as_array()
            .expect("ids")
            .iter()
            .map(|id| id.as_str().expect("an id").to_owned())
            .collect()
    }

    /// A draft for each way `post` plans one, in this order: on a line, a
    /// reply to a posted thread, too far from a change, on lines the head
    /// doesn't have, in a file the PR doesn't change, on a whole file, and
    /// a reply to the draft that can't be posted.
    fn add_planned_drafts(&self) -> Vec<String> {
        let answer = self.apply(&json!([
            {"op": "add", "comment": {"path": "src/lib.rs", "side": "new",
                "start": {"old": null, "new": 13}, "end": {"old": null, "new": 13},
                "anchor": {"before": ["line 11", "line 12"], "lines": ["thirteen"], "after": []},
                "text": "Needs a test.", "distance": 0}},
            {"op": "add", "comment": {"in_reply_to_ref": "github:PRRC_kwDOAAAAB84AAAB1", "text": "Agreed."}},
            {"op": "add", "comment": {"path": "src/lib.rs", "side": "new",
                "start": {"old": 9, "new": 9}, "end": {"old": 10, "new": 10},
                "anchor": {"before": ["line 7", "line 8"], "lines": ["line 9", "line 10"], "after": ["line 11", "line 12"]},
                "text": "Rename these.", "distance": 4}},
            {"op": "add", "comment": {"path": "src/lib.rs", "side": "new",
                "start": {"old": null, "new": 3}, "end": {"old": null, "new": 3},
                "anchor": {"before": ["line 1", "line 2"], "lines": ["not pushed yet"], "after": ["line 3"]},
                "text": "Local only.", "distance": 0}},
            {"op": "add", "comment": {"path": "notes.txt", "side": "new",
                "start": {"old": 1, "new": 1}, "end": {"old": 1, "new": 1},
                "anchor": {"before": [], "lines": ["notes"], "after": []},
                "text": "Typo?", "distance": 0}},
            {"op": "add", "comment": {"path": "src/lib.rs", "text": "Overall fine."}},
        ]));
        let mut ids: Vec<String> = answer["ids"]
            .as_array()
            .expect("ids")
            .iter()
            .map(|id| id.as_str().expect("an id").to_owned())
            .collect();
        let reply = self.apply(&json!([
            {"op": "add", "comment": {"in_reply_to": ids[3], "text": "Me too."}},
        ]));
        ids.push(reply["ids"][0].as_str().expect("an id").to_owned());
        ids
    }

    /// Puts a `git` on the stub's PATH that logs each call, `<args> after
    /// <lines printed>`, for `git_calls`.
    fn log_git(&self) {
        let wrapper = self.path("stub/bin/git");
        fs::write(&wrapper, include_str!("fixtures/stubs/git.sh")).expect("wrapper");
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// The git calls logged since the last `reset_git_calls`.
    fn git_calls(&self) -> Vec<String> {
        fs::read_to_string(self.path("stub/git"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn reset_git_calls(&self) {
        fs::remove_file(self.path("stub/git")).ok();
    }

    /// A commit on the PR's head that only the author has: `src/lib.rs`
    /// with its fifth line `5ive`.
    fn unpushed_commit(&self) -> String {
        let author = self.path("author");
        git(&author, &["checkout", "-q", "-b", "rewritten", &self.head]);
        let lib = fs::read_to_string(author.join("src/lib.rs")).expect("read");
        fs::write(author.join("src/lib.rs"), lib.replace("five\n", "5ive\n")).expect("write");
        git(&author, &["commit", "-q", "-am", "rewritten"]);
        git(&author, &["rev-parse", "HEAD"])
    }

    /// Pushes `commit` to the remote and drops the ref again, as a
    /// force-push leaves a PR's earlier head: there, but on no ref.
    fn orphan_on_remote(&self, commit: &str) {
        let url = self.path("github/acme/widgets.git");
        git(
            &self.path("author"),
            &[
                "push",
                "-q",
                url.to_str().expect("utf-8 path"),
                &format!("{commit}:refs/hidden/orphan"),
            ],
        );
        git(&url, &["update-ref", "-d", "refs/hidden/orphan"]);
        // As GitHub serves them, whatever protocol this git speaks.
        git(&url, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    }

    /// Answers every `PullRequest` with one page: a thread written on
    /// `rewritten`'s fifth line, and one on a commit GitHub no longer has.
    fn reply_force_pushed(&self, rewritten: &str) {
        let thread = |n: u64, commit: &str, line: u64| {
            json!({"id": format!("PRRT_{n}"), "isResolved": false, "isOutdated": true,
                "path": "src/lib.rs", "diffSide": "RIGHT", "startDiffSide": null,
                "originalLine": line, "originalStartLine": null, "subjectType": "LINE",
                "comments": {"nodes": [{"id": format!("PRRC_{n}"), "url": URL,
                    "body": "Hm.", "createdAt": "2026-11-16T09:30:00Z",
                    "viewerDidAuthor": false, "author": {"login": "octocat"},
                    "originalCommit": {"oid": commit}}]}})
        };
        let page = json!({"data": {"repository": {"pullRequest": {
            "title": "Spell out five", "url": URL, "baseRefOid": self.base,
            "headRefOid": self.head, "headRefName": "five",
            "reviewThreads": {"totalCount": 2,
                "pageInfo": {"hasNextPage": false, "endCursor": null},
                "nodes": [thread(1, rewritten, 5), thread(2, GONE, 3)]}}}}});
        fs::write(self.path("stub/gh/PullRequest.json"), page.to_string()).expect("fixture");
    }

    fn draft(&self, id: &str) -> Value {
        self.comments()["comments"]
            .as_array()
            .expect("comments")
            .iter()
            .find(|c| c["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no comment {id}"))
    }
}

/// `initialize` names the command dffy adds, the words its screens use, a
/// PR as `PR #7`, and the events the post dialog offers: `pending` leaves
/// the review open, the others submit it. It watches. dffy starts it from
/// anywhere, so it needs no repository, git or gh to answer.
#[test]
fn initialize_names_the_command_from_nothing_but_itself() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dffy-review-github"));
    cmd.current_dir(dir.path()).env("PATH", "");
    let mut session = Session::start(&mut cmd, dir.path().join("out"));
    let (_, described) = session.call(1, "initialize", &json!({"protocol": 1}));
    let described = described.expect("an answer");
    session.close();
    assert_eq!(described["capabilities"], json!({"watch": true}));
    assert_eq!(
        [
            &described["command"],
            &described["noun"],
            &described["nouns"],
            &described["short"],
            &described["id_prefix"]
        ],
        ["pr", "pull request", "pull requests", "PR", "#"]
    );
    let events: Vec<(&str, &str, bool)> = described["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|e| {
            (
                e["id"].as_str().expect("an id"),
                e["key"].as_str().expect("a key"),
                e["submits"] != false,
            )
        })
        .collect();
    assert_eq!(
        events,
        [
            ("pending", "p", false),
            ("comment", "c", true),
            ("approve", "a", true),
            ("request_changes", "r", true)
        ]
    );
    assert!(described["outdated_reply_warning"].is_string());
}

const NO_GH: &str = "dffy-review-github needs the GitHub CLI (gh): https://cli.github.com";

const LOGGED_OUT: &str = "gh isn't logged in to github.com; run `gh auth login`";

/// A verb that asks GitHub says what's missing when gh isn't installed or
/// isn't logged in, and `prepare` prints no session then.
#[test]
fn gh_missing_or_logged_out_is_said_plainly() {
    let f = Fixture::new();
    for args in [&["list"][..], &["prepare", PR]] {
        let run = f.run_without_gh(args);
        assert_eq!(run.code, Some(1), "{args:?}");
        assert_eq!(run.lines, [json!({"error": NO_GH})], "{args:?}");
    }
    // A token GitHub refuses reads as logged out too.
    fs::write(f.path("stub/gh/bad-token"), "").expect("write");
    let run = f.run(&["list"]);
    assert_eq!(run.lines, [json!({"error": LOGGED_OUT})]);
    f.log_out();
    for args in [&["list"][..], &["prepare", PR]] {
        let run = f.run(args);
        assert_eq!(run.code, Some(1), "{args:?}");
        assert_eq!(run.lines, [json!({"error": LOGGED_OUT})], "{args:?}");
    }
}

/// `doctor` checks gh and its login to the host the API calls go to, and
/// exits 0 whatever they find: the checks carry it.
#[test]
fn doctor_checks_gh_and_its_login() {
    let f = Fixture::new();
    let run = f.run(&["doctor"]);
    run.ok();
    assert_eq!(
        run.lines,
        [json!({"checks": [
            {"name": "gh", "ok": true, "detail": "gh version 2.0.0 (stub)"},
            {"name": "auth", "ok": true, "detail": "Logged in to github.com account octocat (keyring)"},
        ]})]
    );
    assert_eq!(
        fs::read_to_string(f.path("stub/auth")).expect("auth asked"),
        "auth status --hostname github.com\n"
    );
    f.log_out();
    let run = f.run(&["doctor"]);
    run.ok();
    assert_eq!(
        run.lines[0]["checks"][1],
        json!({"name": "auth", "ok": false, "detail": LOGGED_OUT})
    );
    let run = f.run_without_gh(&["doctor"]);
    run.ok();
    assert_eq!(
        run.lines,
        [json!({"checks": [
            {"name": "gh", "ok": false, "detail": NO_GH},
            {"name": "auth", "ok": false, "detail": "needs gh"},
        ]})]
    );
}

#[test]
fn prepare_prints_the_session_once_the_head_is_local_then_imports_the_rest() {
    let f = Fixture::new();
    let run = f.prepare();

    assert_eq!(
        run.lines,
        [
            json!({"session": {"id": "7", "title": "Spell out five", "url": URL,
                "base": f.base, "head": f.head, "last_reviewed_head": null,
                "state": "open"}}),
            json!({"progress": "1/4"}),
            json!({"progress": "4/4"}),
        ]
    );
    // The second page is asked for only after the session line.
    assert_eq!(
        f.events(),
        ["gh PullRequest after 0", "gh PullRequest after 2"]
    );
    // The PR's code came through git.
    assert_eq!(
        git(&f.work(), &["rev-parse", &format!("refs/dffy/pull/{PR}")]),
        f.head
    );

    // GitHub moved this thread to lines 5-6 of a later head; its lines and
    // anchor are those it was written on.
    let root = f.comment("github:PRRC_kwDOAAAAB84AAAB1");
    assert_eq!(root["path"], "src/lib.rs");
    assert_eq!(root["side"], "new");
    assert_eq!(root["start"], json!({"old": null, "new": 4}));
    assert_eq!(root["end"], json!({"old": null, "new": 5}));
    assert_eq!(
        root["anchor"],
        json!({"before": ["line 2", "line 3"], "lines": ["line 4", "five"], "after": ["line 6", "line 7"]})
    );
    assert_eq!(root["text"], "Spell it out.");
    assert_eq!(root["suggestion"], "FIVE");
    assert_eq!(root["author"], "octocat");
    assert_eq!(root["created"], "2026-11-16T09:30:00Z");
    assert_eq!(root["url"], format!("{URL}#discussion_r1001"));
    assert_eq!(root["github_thread"], "PRRT_kwDOAAAAB84AAAA1");
    assert!(root.get("outdated").is_none(), "{root}");

    let reply = f.comment("github:PRRC_kwDOAAAAB84AAAB2");
    assert_eq!(reply["in_reply_to"], root["id"]);
    assert_eq!(reply["author"], "", "the viewer's own comment is yours");
    assert_eq!(reply["url"], format!("{URL}#discussion_r1002"));

    // An old-side thread reads the merge base.
    let old = f.comment("github:PRRC_kwDOAAAAB84AAAB3");
    assert_eq!(old["side"], "old");
    assert_eq!(old["anchor"]["lines"], json!(["line 5"]));
    assert_eq!(old["resolved"], true);

    // An outdated thread reads the commit it was written on.
    let outdated = f.comment("github:PRRC_kwDOAAAAB84AAAB4");
    assert_eq!(outdated["outdated"], true);
    assert_eq!(outdated["end"], json!({"old": null, "new": 2}));
    assert_eq!(outdated["anchor"]["lines"], json!(["line 2"]));
    assert_eq!(outdated["author"], "ghost");

    let file_comment = f.comment("github:PRRC_kwDOAAAAB84AAAB5");
    assert_eq!(file_comment["path"], "notes.txt");
    assert!(file_comment.get("start").is_none() && file_comment.get("anchor").is_none());
}

/// A warm `prepare` prints the kept session first, then says the PR's head
/// moved once GitHub's first page says so, as `check` would.
#[test]
fn a_warm_prepare_answers_from_the_session_before_asking_github() {
    let f = Fixture::new();
    let cold = f.prepare();
    f.reset_events();

    // GitHub has moved on; the session keeps the head dffy showed.
    f.move_head();
    f.reply("PullRequest", Some(3), "pull_request_moved.json");
    let warm = f.run(&["prepare", PR]);
    warm.ok();
    assert_eq!(warm.session(), cold.session());
    assert_eq!(f.events(), ["gh PullRequest after 1"]);
    assert_eq!(f.rest().len(), 1, "the session head's files were kept");
    assert_eq!(
        warm.lines[1..],
        [
            json!({"head": f.head2, "moved": true}),
            json!({"progress": "0/0"})
        ]
    );

    // On the head the session has, nothing moved.
    f.reply("PullRequest", Some(4), "pull_request_page_1.json");
    f.reply("PullRequest", Some(5), "pull_request_page_2.json");
    let unmoved = f.run(&["prepare", PR]);
    unmoved.ok();
    assert!(
        unmoved.lines.iter().all(|l| l.get("moved").is_none()),
        "{:?}",
        unmoved.lines
    );
}

/// The PR below this one merged, and GitHub points this one at another
/// base with the head unchanged: its hunks, kept for the old base, are
/// asked for again.
#[test]
fn a_warm_prepare_on_a_new_base_asks_for_the_files_again() {
    let f = Fixture::new();
    f.prepare();
    f.reply("PullRequest", Some(3), "pull_request_page_1.json");
    let mut retargeted = f.answer("PullRequest.3.json");
    retargeted["data"]["repository"]["pullRequest"]["baseRefOid"] = json!(f.head2);
    fs::write(f.path("stub/gh/PullRequest.3.json"), retargeted.to_string())
        .expect("an answer written");
    f.reply("PullRequest", Some(4), "pull_request_page_2.json");
    f.run(&["prepare", PR]).ok();
    assert_eq!(f.rest().len(), 2, "{:?}", f.rest());
}

/// Watching the PR dffy shows asks GitHub's REST API for it, with the
/// `ETag` of its last answer: a 304, which costs no rate limit, answers
/// from what the last 200 said. It pushes `comments/changed` once the PR
/// changed since the last import, as a new comment changes it, and
/// `head/moved` once its head did, each once. It runs no git fetch,
/// ls-remote or ssh, and no GraphQL.
#[test]
fn watching_asks_github_by_etag_and_never_git() {
    let f = Fixture::new();
    f.prepare();
    f.log_git();
    let before = f.requests().len();
    let imported = "2026-11-16T10:02:11Z";
    f.pull_answer(Some(1), "\"one\"", &f.head, imported);
    f.http(
        "pull",
        Some(2),
        "304 Not Modified",
        &[("Etag", "\"one\"")],
        "",
    );
    f.pull_answer(Some(3), "\"two\"", &f.head, "2026-11-17T08:00:00Z");
    f.http(
        "pull",
        Some(4),
        "304 Not Modified",
        &[("Etag", "\"two\"")],
        "",
    );
    f.pull_answer(Some(5), "\"three\"", &f.head2, "2026-11-17T09:00:00Z");
    f.http(
        "pull",
        Some(6),
        "304 Not Modified",
        &[("Etag", "\"three\"")],
        "",
    );
    let mut watching = f.watch();
    for _ in 0..5 {
        watching.look();
    }
    assert_eq!(
        watching.settled(),
        [
            json!({"method": "comments/changed", "params": {"id": PR}}),
            json!({"method": "head/moved", "params": {"id": PR, "head": f.head2}}),
        ]
    );

    let sent: Vec<String> = f.rest()[1..6].to_vec();
    let path = format!("api --hostname github.com repos/acme/widgets/pulls/{PR} --include");
    assert_eq!(
        sent.iter()
            .map(|r| r.rsplit_once(" after ").expect("a count").0)
            .collect::<Vec<_>>(),
        [
            path.clone(),
            format!("{path} --header If-None-Match: \"one\""),
            format!("{path} --header If-None-Match: \"one\""),
            format!("{path} --header If-None-Match: \"two\""),
            format!("{path} --header If-None-Match: \"two\""),
        ]
    );
    assert_eq!(f.requests().len(), before);
    let git_calls = f.git_calls();
    assert!(
        git_calls
            .iter()
            .all(|c| !c.starts_with("fetch") && !c.starts_with("ls-remote")),
        "{git_calls:?}"
    );
    let ssh = fs::read_to_string(f.path("stub/ssh.log")).expect("prepare fetched");
    assert_eq!(ssh.lines().count(), 1, "prepare's fetch alone: {ssh}");
}

/// A REST answer of a used-up rate limit says when it resets.
#[test]
fn a_used_up_rate_limit_says_when_it_resets() {
    let f = Fixture::new();
    f.prepare();
    // 2026-11-18T14:05:00Z
    f.http(
        "pull",
        None,
        "403 Forbidden",
        &[
            ("X-Ratelimit-Remaining", "0"),
            ("X-Ratelimit-Reset", "1795010700"),
        ],
        r#"{"message":"API rate limit exceeded"}"#,
    );
    let mut watching = f.watch();
    watching.look();
    let said = watching.pushed();
    assert_eq!(said["method"], "log");
    let said = said["params"]["message"].as_str().expect("words");
    assert!(
        said.starts_with("checking #7 failed: ") && said.contains("resets at 14:05 UTC"),
        "{said}"
    );
    assert_eq!(watching.settled(), [] as [Value; 0], "said once");

    let limited = json!({"data": null, "errors": [{"type": "RATE_LIMITED",
        "message": "API rate limit exceeded for user ID 1."}]});
    fs::write(
        f.path("stub/gh/PullRequest.error.json"),
        limited.to_string(),
    )
    .expect("answer");
    f.reply("rate_limit", None, "rate_limit.json");
    let run = f.run(&["pull", PR]);
    assert_eq!(run.code, Some(1));
    assert!(
        run.error().contains("resets at 14:05 UTC"),
        "{}",
        run.error()
    );
}

#[test]
fn pull_moves_the_session_to_the_new_head_and_leaves_the_last_reviewed_one() {
    let f = Fixture::new();
    f.prepare();
    f.move_head();
    f.reply("PullRequest", None, "pull_request_moved.json");

    let run = f.run(&["pull", PR]);
    run.ok();
    assert_eq!(
        run.lines,
        [
            json!({"session": {"id": "7", "title": "Spell out five", "url": URL,
                "base": f.base, "head": f.head2, "last_reviewed_head": null,
                "state": "open"}}),
            json!({"progress": "0/0"}),
        ]
    );
    // What the pull imported is what GitHub has: nothing to push.
    f.pull_answer(None, "\"moved\"", &f.head2, "2026-11-17T08:00:00Z");
    let mut watching = f.watch();
    watching.look();
    assert_eq!(watching.settled(), [] as [Value; 0]);
    assert_eq!(f.run(&["prepare", PR]).session(), run.session());
}

/// Without a head recorded by a `post`, the last reviewed one is your last
/// review's on GitHub, from the first page, when the clone has it.
#[test]
fn the_last_reviewed_head_starts_as_your_last_reviews_on_github() {
    let f = Fixture::new();
    f.reply("PullRequest", Some(1), "pull_request_page_1.json");
    let mut first = f.answer("PullRequest.1.json");
    first["data"]["repository"]["pullRequest"]["latest"] = json!({"nodes": [
        {"viewerDidAuthor": true, "commit": {"oid": f.base}},
        {"viewerDidAuthor": false, "commit": {"oid": f.head}},
    ]});
    fs::write(f.path("stub/gh/PullRequest.1.json"), first.to_string()).expect("fixture");
    f.reply("PullRequest", Some(2), "pull_request_page_2.json");
    f.reply("files", None, "files.json");
    let run = f.run(&["prepare", PR]);
    run.ok();
    assert_eq!(run.session()["last_reviewed_head"], f.base.as_str());
    let asked = &f.requests()[0]["variables"];
    assert_eq!(asked["latest"], true);
    assert_eq!(f.requests()[1]["variables"]["latest"], false);

    // One the clone doesn't have isn't taken.
    let mut gone = first.clone();
    gone["data"]["repository"]["pullRequest"]["latest"]["nodes"][0]["commit"]["oid"] = json!(GONE);
    fs::write(f.path("stub/gh/PullRequest.3.json"), gone.to_string()).expect("fixture");
    f.reply("PullRequest", Some(4), "pull_request_page_2.json");
    let mut session = f.saved_session();
    session["last_reviewed_head"] = Value::Null;
    f.save_session(&session);
    f.run(&["pull", PR]).ok();
    assert_eq!(f.saved_session()["last_reviewed_head"], Value::Null);
}

/// `post` records the head it posted against as the last reviewed one,
/// once it finished; pulls after it keep it.
#[test]
fn a_finished_post_records_the_head_it_reviewed() {
    let f = Fixture::new();
    f.prepare();
    f.add_drafts();
    f.reply("PostState", Some(1), "post_state.json");
    f.reply("PostState", Some(2), "post_state_pending.json");
    f.reply("AddReview", None, "add_review.json");
    f.reply("AddPullRequestReviewThread", Some(1), "add_thread_1.json");
    f.down("AddPullRequestReviewThread", Some(2));
    f.reply("AddPullRequestReviewThread", Some(3), "add_thread_2.json");
    f.reply("AddPullRequestReviewThreadReply", None, "add_reply.json");
    f.reply("SubmitPullRequestReview", None, "submit.json");

    assert_eq!(f.post("comment", "").code, Some(1));
    assert_eq!(f.saved_session()["last_reviewed_head"], Value::Null);
    f.post("comment", "").ok();
    assert_eq!(f.saved_session()["last_reviewed_head"], f.head.as_str());

    f.move_head();
    f.reply("PullRequest", None, "pull_request_moved.json");
    let run = f.run(&["pull", PR]);
    run.ok();
    assert_eq!(run.session()["head"], f.head2.as_str());
    assert_eq!(run.session()["last_reviewed_head"], f.head.as_str());
}

#[test]
fn post_sends_drafts_replies_and_resolves_once_and_a_second_post_sends_nothing() {
    let f = Fixture::new();
    f.prepare();
    let ids = f.add_drafts();
    f.apply(&json!([{"op": "set", "ref": "github:PRRC_kwDOAAAAB84AAAB1", "fields": {"resolved": true}}]));
    f.reply("PostState", None, "post_state.json");
    f.reply("AddReview", None, "add_review.json");
    f.reply("AddPullRequestReviewThread", Some(1), "add_thread_1.json");
    f.reply("AddPullRequestReviewThread", Some(2), "add_thread_2.json");
    f.reply("AddPullRequestReviewThreadReply", None, "add_reply.json");
    f.reply("ResolveReviewThread", None, "resolve.json");
    f.reply("SubmitPullRequestReview", None, "submit.json");

    let run = f.post("comment", "Ship it\n");
    run.ok();
    assert_eq!(
        run.lines,
        [
            json!({"progress": "1/3"}),
            json!({"progress": "2/3"}),
            json!({"progress": "3/3"}),
            json!({"last_reviewed_head": f.head, "posted": 3, "failed": 0, "pending": 0,
                "url": format!("{URL}#pullrequestreview-2001")}),
        ]
    );

    let sent = f.operations();
    assert_eq!(
        sent[2..],
        [
            "PostState",
            "AddReview",
            "AddPullRequestReviewThread",
            "AddPullRequestReviewThreadReply",
            "AddPullRequestReviewThread",
            "ResolveReviewThread",
            "SubmitPullRequestReview",
        ]
    );
    let requests = f.requests();
    let vars = |i: usize| requests[i]["variables"].clone();
    assert_eq!(
        vars(3)["commit"],
        f.head.as_str(),
        "the review is on the session's head"
    );
    assert_eq!(
        vars(4),
        json!({"review": "PRR_kwDOAAAAB84AAAAR", "path": "src/lib.rs", "body": "Needs a test.", "line": 13, "side": "RIGHT"})
    );
    assert_eq!(
        vars(5),
        json!({"review": "PRR_kwDOAAAAB84AAAAR", "thread": "PRRT_kwDOAAAAB84AAAA1", "body": "Agreed."})
    );
    // Too far from a change for GitHub: a file comment quoting the lines.
    let far = vars(6);
    assert_eq!(far["subjectType"], "FILE");
    assert!(far.get("line").is_none());
    assert_eq!(
        far["body"],
        "Lines 9-10:\n```\nline 9\nline 10\n```\n\nRename these."
    );
    assert_eq!(vars(7), json!({"thread": "PRRT_kwDOAAAAB84AAAA1"}));
    assert_eq!(
        vars(8),
        json!({"review": "PRR_kwDOAAAAB84AAAAR", "event": "COMMENT", "body": "Ship it"})
    );

    let posted = f.draft(&ids[0]);
    assert_eq!(posted["ref"], "github:PRRC_kwDOAAAAB84AAAC1");
    assert_eq!(posted["github_thread"], "PRRT_kwDOAAAAB84AAAB1");
    assert_eq!(posted["url"], format!("{URL}#discussion_r1101"));
    assert!(posted.get("pending").is_none(), "submitted: {posted}");
    assert_eq!(f.draft(&ids[1])["ref"], "github:PRRC_kwDOAAAAB84AAAC3");
    assert_eq!(f.draft(&ids[1])["url"], format!("{URL}#discussion_r1103"));
    assert_eq!(f.draft(&ids[2])["ref"], "github:PRRC_kwDOAAAAB84AAAC2");
    assert_eq!(
        f.comment("github:PRRC_kwDOAAAAB84AAAB1")["host_resolved"],
        true
    );

    // A retry finds nothing left to send, and points at the PR.
    let before = f.operations().len();
    let run = f.post("pending", "");
    run.ok();
    assert_eq!(
        run.lines,
        [json!({"posted": 0, "failed": 0, "pending": 0, "url": URL})]
    );
    assert_eq!(f.operations()[before..], ["PostState"]);
}

/// A draft GitHub refuses stays a draft, with GitHub's reason in its
/// `post_error` and in its draft line, and the review is submitted with
/// the event anyway, without it. A retry posts it in a new review, and its
/// `post_error` goes.
#[test]
fn a_refused_comment_stays_a_draft_saying_why_and_the_review_goes_without_it() {
    let f = Fixture::new();
    f.prepare();
    let ids = f.add_drafts();
    f.reply("PostState", None, "post_state.json");
    f.reply("AddReview", None, "add_review.json");
    f.reply(
        "AddPullRequestReviewThread",
        Some(1),
        "add_thread_rejected.json",
    );
    f.reply("AddPullRequestReviewThread", Some(2), "add_thread_2.json");
    f.reply("AddPullRequestReviewThread", Some(3), "add_thread_1.json");
    f.reply("AddPullRequestReviewThreadReply", None, "add_reply.json");
    f.reply("SubmitPullRequestReview", None, "submit.json");

    let run = f.post("approve", "");
    run.ok();
    assert_eq!(
        run.lines[run.lines.len() - 2..],
        [
            json!({"draft": {"id": ids[0], "path": "src/lib.rs", "plan": "skip",
                "reason": "Line could not be resolved"}}),
            json!({"last_reviewed_head": f.head, "posted": 2, "failed": 1, "pending": 0,
                "url": format!("{URL}#pullrequestreview-2001")}),
        ]
    );
    let rejected = f.draft(&ids[0]);
    assert!(rejected.get("ref").is_none(), "{rejected}");
    assert_eq!(rejected["text"], "Needs a test.");
    assert_eq!(rejected["post_error"], "Line could not be resolved");
    assert_eq!(f.draft(&ids[2])["ref"], "github:PRRC_kwDOAAAAB84AAAC2");
    assert!(
        f.comments().get("notice").is_none(),
        "the reason is the draft's"
    );
    assert_eq!(
        f.requests().last().expect("the submit")["variables"],
        json!({"review": "PRR_kwDOAAAAB84AAAAR", "event": "APPROVE"})
    );
    assert_eq!(f.saved_session()["last_reviewed_head"], f.head.as_str());

    let run = f.post("approve", "");
    run.ok();
    assert_eq!(
        *run.last(),
        json!({"last_reviewed_head": f.head, "posted": 1, "failed": 0, "pending": 0,
            "url": format!("{URL}#pullrequestreview-2001")})
    );
    let posted = f.draft(&ids[0]);
    assert_eq!(posted["ref"], "github:PRRC_kwDOAAAAB84AAAC1");
    assert!(posted.get("post_error").is_none(), "{posted}");
    let sent = f.operations();
    let count = |op: &str| sent.iter().filter(|o| *o == op).count();
    assert_eq!(count("AddReview"), 2);
    assert_eq!(count("AddPullRequestReviewThread"), 3);
    assert_eq!(count("AddPullRequestReviewThreadReply"), 1);
    assert_eq!(count("SubmitPullRequestReview"), 2);
}

#[test]
fn a_post_stopped_midway_fails_and_a_retry_resumes_without_posting_twice() {
    let f = Fixture::new();
    f.prepare();
    let ids = f.add_drafts();
    f.reply("PostState", Some(1), "post_state.json");
    f.reply("PostState", Some(2), "post_state_pending.json");
    f.reply("AddReview", None, "add_review.json");
    f.reply("AddPullRequestReviewThread", Some(1), "add_thread_1.json");
    f.down("AddPullRequestReviewThread", Some(2));
    f.reply("AddPullRequestReviewThread", Some(3), "add_thread_2.json");
    f.reply("AddPullRequestReviewThreadReply", None, "add_reply.json");
    f.reply("SubmitPullRequestReview", None, "submit.json");

    let run = f.post("comment", "");
    assert_eq!(run.code, Some(1));
    assert_eq!(
        run.lines[run.lines.len() - 2],
        json!({"posted": 2, "failed": 1, "pending": 0,
            "url": format!("{URL}#pullrequestreview-2001")})
    );
    assert!(run.error().contains("api.github.com"), "{}", run.error());
    assert!(f.draft(&ids[2]).get("ref").is_none());
    // Not submitted: they wait in your pending review.
    assert_eq!(f.draft(&ids[0])["pending"], true);
    assert!(f.draft(&ids[2]).get("post_error").is_none(), "not refused");

    let run = f.post("comment", "");
    run.ok();
    assert_eq!(
        *run.last(),
        json!({"last_reviewed_head": f.head, "posted": 1, "failed": 0, "pending": 2,
            "url": format!("{URL}#pullrequestreview-2001")})
    );
    assert!(f.draft(&ids[0]).get("pending").is_none(), "submitted");
    let sent = f.operations();
    let count = |op: &str| sent.iter().filter(|o| *o == op).count();
    assert_eq!(count("AddReview"), 1);
    assert_eq!(count("AddPullRequestReviewThread"), 3);
    assert_eq!(count("AddPullRequestReviewThreadReply"), 1);
    assert_eq!(count("SubmitPullRequestReview"), 1);
}

#[test]
fn an_api_failure_keeps_every_comment_and_says_why() {
    let f = Fixture::new();
    f.prepare();
    f.add_drafts();
    f.apply(&json!([{"op": "set", "ref": "github:PRRC_kwDOAAAAB84AAAB1", "fields": {"resolved": true}}]));
    let before = f.comments()["comments"].clone();

    f.down("PullRequest", None);
    let run = f.run(&["pull", PR]);
    assert_eq!(run.code, Some(1));
    assert!(run.error().contains("api.github.com"), "{}", run.error());
    assert_eq!(f.comments()["comments"], before);

    f.down("PostState", None);
    let run = f.post("comment", "");
    assert_eq!(run.code, Some(1));
    assert_eq!(run.lines.len(), 1);
    assert!(run.error().contains("api.github.com"), "{}", run.error());
    let after = f.comments();
    assert_eq!(after["comments"], before);
    let notice = after["notice"].as_str().expect("a notice");
    assert!(notice.contains("nothing was posted"), "{notice}");

    // A pull that works again keeps the unsent resolve and the drafts.
    f.up("PullRequest");
    f.reply("PullRequest", Some(4), "pull_request_page_1.json");
    f.reply("PullRequest", Some(5), "pull_request_page_2.json");
    f.run(&["pull", PR]).ok();
    let root = f.comment("github:PRRC_kwDOAAAAB84AAAB1");
    assert_eq!(root["resolved"], true);
    assert_eq!(root["host_resolved"], false);
    let after = f.comments();
    let drafts = |c: &Value| -> Vec<Value> {
        c.as_array()
            .expect("comments")
            .iter()
            .filter(|c| c.get("ref").is_none())
            .cloned()
            .collect()
    };
    assert_eq!(drafts(&after["comments"]), drafts(&before));
    assert_eq!(
        after["comments"].as_array().map(Vec::len),
        before.as_array().map(Vec::len)
    );
}

#[test]
fn the_code_opens_when_the_api_is_down() {
    let f = Fixture::new();
    f.down("PullRequest", None);
    let run = f.run(&["prepare", PR]);
    assert_eq!(run.code, Some(1));
    // The base is the default branch's tip, the head the fetched PR ref.
    assert_eq!(run.session()["base"], f.base.as_str());
    assert_eq!(run.session()["head"], f.head.as_str());
    assert_eq!(run.lines.len(), 2);
    assert!(run.error().contains("api.github.com"), "{}", run.error());

    // That base was a guess, so the next prepare asks GitHub again.
    f.up("PullRequest");
    f.reply("PullRequest", Some(2), "pull_request_page_1.json");
    f.reply("PullRequest", Some(3), "pull_request_page_2.json");
    let run = f.run(&["prepare", PR]);
    run.ok();
    assert_eq!(run.session()["title"], "Spell out five");

    // A pull with GitHub down again opens the head just fetched, with the
    // rest of what was kept.
    f.move_head();
    f.down("PullRequest", Some(4));
    let down = f.run(&["pull", PR]);
    assert_eq!(down.code, Some(1));
    let mut expected = run.session().clone();
    expected["head"] = json!(f.head2);
    assert_eq!(*down.session(), expected);
    assert_eq!(f.saved_session()["state"], "OPEN");
}

/// `post` puts each draft on the lines its anchor has at the PR's head:
/// one written where the working tree had two more lines above moves up,
/// and one on lines the head doesn't have stays a draft, saying why, and
/// doesn't hold the review back.
#[test]
fn post_places_drafts_by_their_anchor_at_the_pr_head() {
    let f = Fixture::new();
    f.prepare();
    let answer = f.apply(&json!([
        {"op": "add", "comment": {"path": "src/lib.rs", "side": "new",
            "start": {"old": null, "new": 15}, "end": {"old": null, "new": 15},
            "anchor": {"before": ["line 11", "line 12"], "lines": ["thirteen"], "after": []},
            "text": "Needs a test.", "distance": 0}},
        {"op": "add", "comment": {"path": "src/lib.rs", "side": "new",
            "start": {"old": null, "new": 3}, "end": {"old": null, "new": 3},
            "anchor": {"before": ["line 1", "line 2"], "lines": ["not pushed yet"], "after": ["line 3"]},
            "text": "Local only.", "distance": 0}},
    ]));
    let local = answer["ids"][1].as_str().expect("an id").to_owned();
    f.reply("PostState", None, "post_state.json");
    f.reply("AddReview", None, "add_review.json");
    f.reply("AddPullRequestReviewThread", None, "add_thread_1.json");
    f.reply("SubmitPullRequestReview", None, "submit.json");

    let run = f.post("comment", "");
    run.ok();
    assert_eq!(
        run.lines[run.lines.len() - 2..],
        [
            json!({"draft": {"id": local, "path": "src/lib.rs", "plan": "skip",
                "reason": "lines not in the PR head (unpushed changes?)"}}),
            json!({"last_reviewed_head": f.head, "posted": 1, "failed": 1, "pending": 0,
                "url": format!("{URL}#pullrequestreview-2001")}),
        ]
    );
    assert_eq!(
        f.operations().last().map(String::as_str),
        Some("SubmitPullRequestReview"),
        "submitted without the draft the head doesn't have"
    );
    let thread = f
        .requests()
        .into_iter()
        .find(|r| r["operationName"] == "AddPullRequestReviewThread")
        .expect("a thread");
    assert_eq!(thread["variables"]["line"], 13);
    let skipped = f.draft(&local);
    assert!(skipped.get("ref").is_none());
    assert!(
        skipped.get("post_error").is_none(),
        "planned out, not refused"
    );
    assert!(
        f.comments().get("notice").is_none(),
        "its draft line says why"
    );
    assert_eq!(
        f.saved_session()["last_reviewed_head"],
        f.head.as_str(),
        "one posted"
    );
}

/// `--dry-run` says what `post` would do with each draft, from git and
/// what `prepare` kept: on its line at the PR's head, as a file comment, or
/// not at all and why. Nothing is asked of GitHub and nothing is written.
#[test]
fn a_dry_run_plans_each_draft_without_asking_github() {
    let f = Fixture::new();
    f.prepare();
    let ids = f.add_planned_drafts();
    let before = (f.requests().len(), f.rest().len(), f.comments());

    let run = f.run(&["post", PR, "--dry-run"]);
    run.ok();
    let draft = |i: usize, plan: Value| {
        let mut draft = json!({"id": ids[i], "path": "src/lib.rs"});
        draft
            .as_object_mut()
            .expect("an object")
            .extend(plan.as_object().expect("an object").clone());
        json!({ "draft": draft })
    };
    assert_eq!(
        run.lines,
        [
            draft(0, json!({"plan": "line", "line": 13, "side": "new"})),
            draft(1, json!({"plan": "line"})),
            draft(2, json!({"plan": "file", "line": 10, "side": "new"})),
            draft(
                3,
                json!({"plan": "skip", "reason": "lines not in the PR head (unpushed changes?)"})
            ),
            draft(
                4,
                json!({"path": "notes.txt", "plan": "skip", "reason": "file not in the PR"})
            ),
            draft(5, json!({"plan": "file"})),
            draft(
                6,
                json!({"plan": "skip", "reason": "its thread isn't posted"})
            ),
        ]
    );
    assert_eq!((f.requests().len(), f.rest().len(), f.comments()), before);
}

/// A reply under a comment kept local, as a mention that asked an agent,
/// has no thread on GitHub, and won't until it's yours: dffy doesn't send
/// it, so neither a dry run nor `post` plans it, and GitHub gets nothing
/// for it.
#[test]
fn a_reply_to_a_comment_kept_local_is_never_sent() {
    let f = Fixture::new();
    f.prepare();
    let root = f.apply(&json!([
        {"op": "add", "comment": {"path": "src/lib.rs", "text": "@claude why?", "local_only": true}},
    ]));
    let reply = f.apply(&json!([
        {"op": "add", "comment": {"in_reply_to": root["ids"][0], "text": "And its tests?"}},
    ]));
    let reply = reply["ids"][0].as_str().expect("an id").to_owned();
    let run = f.run(&["post", PR, "--dry-run"]);
    run.ok();
    assert_eq!(run.lines, Vec::<Value>::new());

    f.reply("PostState", None, "post_state.json");
    let before = f.operations().len();
    let run = f.post("pending", "");
    run.ok();
    assert_eq!(
        run.lines,
        [json!({"posted": 0, "failed": 0, "pending": 0, "url": URL})]
    );
    assert_eq!(f.operations()[before..], ["PostState"]);
    assert!(f.draft(&reply).get("ref").is_none());
}

/// `--drafts` plans another comments file's drafts after the PR's own, as
/// `post` would once they're moved in, writing neither file; what's
/// pending and the resolves waiting don't change a plan.
#[test]
fn a_dry_run_plans_drafts_from_another_file() {
    let f = Fixture::new();
    f.prepare();
    f.apply(&json!([
        {"op": "set", "ref": "github:PRRC_kwDOAAAAB84AAAB1", "fields": {"resolved": true}},
        {"op": "upsert", "comment": {"ref": "github:PRRC_kwDOAAAAB84AAAB2", "pending": true}},
    ]));
    let local = f.path("local.json");
    let mut child = Command::new(&f.real_dffy)
        .arg("--comments")
        .arg(&local)
        .args(["comments", "apply"])
        .env("XDG_CONFIG_HOME", f.path("config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("dffy runs");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(
            json!([{"op": "add", "comment": {"path": "src/lib.rs", "side": "new",
                "start": {"old": null, "new": 13}, "end": {"old": null, "new": 13},
                "anchor": {"before": ["line 11", "line 12"], "lines": ["thirteen"], "after": []},
                "text": "Needs a test.", "distance": 0}}])
            .to_string()
            .as_bytes(),
        )
        .expect("ops written");
    let out = child.wait_with_output().expect("dffy exits");
    let id = serde_json::from_slice::<Value>(&out.stdout).expect("JSON")["ids"][0].clone();
    let before = (f.comments(), fs::read(&local).expect("the local file"));

    let run = f.run(&[
        "post",
        PR,
        "--dry-run",
        "--drafts",
        local.to_str().expect("utf-8"),
    ]);
    run.ok();
    assert_eq!(
        run.lines,
        [
            json!({"draft": {"id": id, "path": "src/lib.rs", "plan": "line", "line": 13, "side": "new"}}),
        ]
    );
    assert_eq!(
        (f.comments(), fs::read(&local).expect("the local file")),
        before
    );
}

/// `post` does what `--dry-run` said, draft by draft: the same drafts are
/// left out for the same reasons, and the rest go on a line or the file
/// as planned. Drafts planned out stay drafts, and the event goes.
#[test]
fn post_follows_the_dry_runs_plan() {
    let f = Fixture::new();
    f.prepare();
    let ids = f.add_planned_drafts();
    let planned = f.run(&["post", PR, "--dry-run"]);
    planned.ok();
    f.reply("PostState", None, "post_state.json");
    f.reply("AddReview", None, "add_review.json");
    f.reply("AddPullRequestReviewThread", Some(1), "add_thread_1.json");
    f.reply("AddPullRequestReviewThread", Some(2), "add_thread_2.json");
    f.reply("AddPullRequestReviewThread", Some(3), "add_thread_3.json");
    f.reply("AddPullRequestReviewThreadReply", None, "add_reply.json");
    f.reply("SubmitPullRequestReview", None, "submit.json");

    let run = f.post("comment", "");
    run.ok();
    let plans: Vec<&Value> = planned
        .lines
        .iter()
        .filter_map(|l| l.get("draft"))
        .collect();
    let skipped = |lines: &[Value]| -> Vec<Value> {
        lines
            .iter()
            .filter(|l| l["draft"]["plan"] == "skip")
            .cloned()
            .collect()
    };
    assert_eq!(skipped(&run.lines), skipped(&planned.lines));
    let requests = f.requests();
    let threads: Vec<&Value> = requests
        .iter()
        .filter(|r| r["operationName"] == "AddPullRequestReviewThread")
        .map(|r| &r["variables"])
        .collect();
    let new_threads: Vec<&Value> = plans
        .iter()
        .copied()
        .filter(|p| p["plan"] != "skip" && p["id"] != ids[1].as_str())
        .collect();
    assert_eq!(threads.len(), new_threads.len());
    for (plan, vars) in new_threads.iter().zip(&threads) {
        if plan["plan"] == "line" {
            assert_eq!(vars["line"], plan["line"], "{vars}");
            assert!(vars.get("subjectType").is_none(), "{vars}");
        } else {
            assert_eq!(vars["subjectType"], "FILE", "{vars}");
        }
    }
    let replies = requests
        .iter()
        .filter(|r| r["operationName"] == "AddPullRequestReviewThreadReply")
        .count();
    assert_eq!(replies, 1);
    assert_eq!(
        *run.last(),
        json!({"last_reviewed_head": f.head, "posted": 4, "failed": 3, "pending": 0,
            "url": format!("{URL}#pullrequestreview-2001")})
    );
    assert_eq!(
        f.operations().last().map(String::as_str),
        Some("SubmitPullRequestReview"),
        "drafts planned out don't hold the event back"
    );
}

/// Drafts whose stored `distance` was measured against another diff: each
/// is planned, on either side, by where its lines are in the PR's own
/// hunks, and in a renamed file by its hunks under the new name. Each one
/// is `(draft, plan, line)`.
fn add_mismeasured_drafts(f: &Fixture) -> Vec<(String, &'static str, u64)> {
    let draft = |path: &str, side: &str, (start, end): (u64, u64), lines: &[&str], distance| {
        let at = |line| {
            let mut both = json!({"old": null, "new": null});
            both[side] = json!(line);
            both
        };
        let mut draft = json!({"path": path, "side": side, "start": at(start), "end": at(end),
            "anchor": {"before": [], "lines": lines, "after": []},
            "text": "Hm.", "distance": distance});
        if path == "docs/new name.txt" {
            draft["old_path"] = json!("docs/old name.txt");
        }
        json!({"op": "add", "comment": draft})
    };
    let planned = [
        (
            draft("src/long.rs", "new", (17, 17), &["long 15"], 0),
            "file",
            17,
        ),
        (
            draft("src/long.rs", "new", (32, 32), &["thirty"], 50),
            "line",
            32,
        ),
        // In the hunk on the old side (27 to 33), not on the new (29 to 35).
        (
            draft("src/long.rs", "old", (28, 28), &["long 28"], 50),
            "line",
            28,
        ),
        (
            draft("src/long.rs", "old", (12, 12), &["long 12"], 0),
            "file",
            12,
        ),
        // Its last line is in the hunk, its first isn't.
        (
            draft("src/long.rs", "new", (28, 29), &["long 26", "long 27"], 0),
            "file",
            29,
        ),
        (
            draft("docs/new name.txt", "old", (10, 10), &["moved 10"], 50),
            "line",
            10,
        ),
        (
            draft("docs/new name.txt", "new", (10, 10), &["ten"], 50),
            "line",
            10,
        ),
        (
            draft("docs/new name.txt", "new", (2, 2), &["moved 2"], 0),
            "file",
            2,
        ),
    ];
    let ops: Vec<Value> = planned.iter().map(|(op, ..)| op.clone()).collect();
    let answer = f.apply(&json!(ops));
    planned
        .iter()
        .zip(answer["ids"].as_array().expect("ids"))
        .map(|((_, plan, line), id)| (id.as_str().expect("an id").to_owned(), *plan, *line))
        .collect()
}

/// A draft's stored `distance` is a hint from the diff it was written on;
/// both `--dry-run` and `post` measure it against the PR's diff instead.
#[test]
fn drafts_go_on_their_lines_by_the_pr_diff_not_their_stored_distance() {
    let f = Fixture::new();
    f.prepare();
    let drafts = add_mismeasured_drafts(&f);

    let planned = f.run(&["post", PR, "--dry-run"]);
    planned.ok();
    let plans: Vec<(String, &str, u64)> = planned
        .lines
        .iter()
        .filter_map(|l| l.get("draft"))
        .map(|d| {
            (
                d["id"].as_str().expect("an id").to_owned(),
                d["plan"].as_str().expect("a plan"),
                d["line"].as_u64().expect("a line"),
            )
        })
        .collect();
    assert_eq!(plans, drafts);

    f.reply("PostState", None, "post_state.json");
    f.reply("AddReview", None, "add_review.json");
    f.reply("SubmitPullRequestReview", None, "submit.json");
    for n in 1..=drafts.len() {
        fs::write(
            f.path("stub/gh")
                .join(format!("AddPullRequestReviewThread.{n}.json")),
            json!({"data": {"addPullRequestReviewThread": {"thread": {
                "id": format!("PRRT_{n}"),
                "comments": {"nodes": [{"id": format!("PRRC_{n}"), "url": URL}]}}}}})
            .to_string(),
        )
        .expect("fixture written");
    }
    f.post("comment", "").ok();
    let sent: Vec<(&str, u64)> = f
        .requests()
        .iter()
        .filter(|r| r["operationName"] == "AddPullRequestReviewThread")
        .map(|r| {
            let vars = &r["variables"];
            if vars["subjectType"] == "FILE" {
                ("file", 0)
            } else {
                ("line", vars["line"].as_u64().expect("a line"))
            }
        })
        .collect();
    let expected: Vec<(&str, u64)> = drafts
        .iter()
        .map(|(_, plan, line)| (*plan, if *plan == "line" { *line } else { 0 }))
        .collect();
    assert_eq!(sent, expected);
}

/// Planning reads the PR's diff once a run, however many drafts there are.
#[test]
fn a_dry_runs_git_calls_dont_grow_with_its_drafts() {
    let f = Fixture::new();
    f.prepare();
    f.log_git();
    add_mismeasured_drafts(&f);
    f.run(&["post", PR, "--dry-run"]).ok();
    let once = f.git_calls().len();
    assert!(once > 0, "git was run");
    f.reset_git_calls();
    for _ in 0..3 {
        add_mismeasured_drafts(&f);
    }
    f.run(&["post", PR, "--dry-run"]).ok();
    assert_eq!(f.git_calls().len(), once);
}

/// An original commit GitHub never had, or has dropped.
const GONE: &str = "0123456789abcdef0123456789abcdef01234567";

/// A thread is anchored on its original commit, which a force-push can
/// leave on no ref, so fetching the PR's head doesn't bring it. Those
/// commits are fetched by oid, all in one fetch after the session line;
/// one GitHub doesn't have leaves its thread a file comment that says why.
/// Once placed, a thread keeps its lines without the commit.
#[test]
fn threads_on_force_pushed_commits_are_placed_once_fetched_else_say_why() {
    let f = Fixture::new();
    let rewritten = f.unpushed_commit();
    f.reply_force_pushed(&rewritten);
    f.reply("files", None, "files.json");
    f.log_git();
    let unplaced = json!("original commit not available");

    // Neither commit is anywhere the plugin can fetch from.
    f.run(&["prepare", PR]).ok();
    for reference in ["github:PRRC_1", "github:PRRC_2"] {
        let thread = f.comment(reference);
        assert_eq!(thread["github_unplaced"], unplaced, "{thread}");
        assert!(thread.get("anchor").is_none(), "{thread}");
    }

    f.orphan_on_remote(&rewritten);
    let work = f.work();
    let in_clone = || {
        Command::new("git")
            .current_dir(&work)
            .args(["cat-file", "-e", &format!("{rewritten}^{{commit}}")])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    assert!(!in_clone(), "the clone doesn't have it yet");
    f.reset_git_calls();
    let warm = f.run(&["prepare", PR]);
    warm.ok();
    let placed = f.comment("github:PRRC_1");
    assert_eq!(placed["end"], json!({"old": null, "new": 5}));
    assert_eq!(
        placed["anchor"],
        json!({"before": ["line 3", "line 4"], "lines": ["5ive"], "after": ["line 6", "line 7"]})
    );
    assert!(placed.get("github_unplaced").is_none(), "{placed}");
    let gone = f.comment("github:PRRC_2");
    assert_eq!(gone["github_unplaced"], unplaced);
    assert!(gone.get("anchor").is_none(), "{gone}");

    // Both in one fetch, after the session line; origin refuses the whole
    // fetch for the commit it lacks, so the other is fetched again alone.
    let logged_fetches = || -> Vec<(String, usize)> {
        f.git_calls()
            .iter()
            .filter(|c| c.starts_with("fetch "))
            .map(|c| {
                let (call, printed) = c.rsplit_once(" after ").expect("a count");
                (call.to_owned(), printed.parse().expect("a count"))
            })
            .collect()
    };
    let fetched = logged_fetches();
    let calls: Vec<&str> = fetched.iter().map(|(c, _)| c.as_str()).collect();
    let mut both = [GONE, rewritten.as_str()];
    both.sort_unstable();
    assert_eq!(
        calls,
        [
            format!(
                "fetch --quiet --no-tags --end-of-options origin {} {}",
                both[0], both[1]
            ),
            format!("fetch --quiet --no-tags --end-of-options origin {rewritten}"),
        ]
    );
    assert!(fetched[0].1 >= 1, "fetched before the session line");

    // GitHub drops the commit, and the clone's copy is pruned: the thread
    // keeps the lines it was placed on, and nothing is fetched for it.
    let remote = f.path("github/acme/widgets.git");
    git(&remote, &["gc", "-q", "--prune=now"]);
    git(&work, &["gc", "-q", "--prune=now"]);
    assert!(!in_clone(), "pruned");
    f.reset_git_calls();
    f.run(&["prepare", PR]).ok();
    assert_eq!(f.comment("github:PRRC_1"), placed);
    let fetched = logged_fetches();
    let calls: Vec<&str> = fetched.iter().map(|(c, _)| c.as_str()).collect();
    assert_eq!(
        calls,
        [format!(
            "fetch --quiet --no-tags --end-of-options origin {GONE}"
        )]
    );
}

/// When every thread's original commit is in the clone, importing them
/// costs no git call beyond reading the files.
#[test]
fn a_prepare_with_every_original_commit_local_fetches_nothing() {
    let f = Fixture::new();
    f.prepare();
    f.log_git();
    f.reply("PullRequest", Some(3), "pull_request_page_1.json");
    f.reply("PullRequest", Some(4), "pull_request_page_2.json");
    f.run(&["prepare", PR]).ok();
    let calls: Vec<String> = f
        .git_calls()
        .iter()
        .map(|c| {
            c.rsplit_once(" after ")
                .map_or(c.as_str(), |(c, _)| c)
                .to_owned()
        })
        .collect();
    let (base, head) = (&f.base, &f.head);
    assert_eq!(
        calls,
        [
            r"config --get-regexp ^remote\..*\.gh-resolved$".to_owned(),
            "remote -v".to_owned(),
            "cat-file --batch".to_owned(),
            format!("merge-base --end-of-options {base} {head}"),
        ]
    );
}

/// The PR's files come from GitHub, every page of them, and GitHub's own
/// hunks decide line or file where git's would differ: a draft outside
/// git's hunk but inside GitHub's goes on its line, and so does an old-side
/// one in a renamed file. A file GitHub sent without its patch is planned
/// from git's diff, and its draft says `unsure`; a file comment never does.
#[test]
fn drafts_are_planned_by_githubs_hunks_and_by_gits_where_github_left_the_patch_out() {
    let f = Fixture::new();
    f.reply("files", Some(1), "files_paged.json");
    f.prepare();
    let rest = f.rest();
    assert_eq!(rest.len(), 1, "{rest:?}");
    assert!(
        rest[0].starts_with(&format!(
            "api --hostname github.com repos/acme/widgets/pulls/{PR}/files?per_page=100 --paginate after "
        )),
        "{rest:?}"
    );
    let answer = f.apply(&json!([
        {"op": "add", "comment": {"path": "src/long.rs", "side": "new",
            "start": {"old": null, "new": 17}, "end": {"old": null, "new": 17},
            "anchor": {"before": [], "lines": ["long 15"], "after": []},
            "text": "Hm.", "distance": 0}},
        {"op": "add", "comment": {"path": "docs/new name.txt", "old_path": "docs/old name.txt",
            "side": "old", "start": {"old": 10, "new": null}, "end": {"old": 10, "new": null},
            "anchor": {"before": [], "lines": ["moved 10"], "after": []},
            "text": "Hm.", "distance": 0}},
        {"op": "add", "comment": {"path": "src/lib.rs", "side": "new",
            "start": {"old": null, "new": 13}, "end": {"old": null, "new": 13},
            "anchor": {"before": ["line 11", "line 12"], "lines": ["thirteen"], "after": []},
            "text": "Needs a test.", "distance": 0}},
        {"op": "add", "comment": {"path": "src/lib.rs", "text": "Overall fine."}},
    ]));
    let id = |i: usize| answer["ids"][i].clone();

    let run = f.run(&["post", PR, "--dry-run"]);
    run.ok();
    assert_eq!(
        run.lines,
        [
            json!({"draft": {"id": id(0), "path": "src/long.rs", "plan": "line", "line": 17, "side": "new"}}),
            json!({"draft": {"id": id(1), "path": "docs/new name.txt", "plan": "line", "line": 10, "side": "old"}}),
            json!({"draft": {"id": id(2), "path": "src/lib.rs", "plan": "line", "line": 13, "side": "new",
                "unsure": true}}),
            json!({"draft": {"id": id(3), "path": "src/lib.rs", "plan": "file"}}),
        ]
    );
}

/// GitHub's hunks are kept for the head they're the diff of. A warm
/// `prepare` that finds the PR moved on keeps those of the session's head;
/// after a `pull` to a head whose files couldn't be fetched, drafts are
/// planned from git's diff, `unsure`.
#[test]
fn githubs_hunks_plan_only_the_head_they_were_fetched_for() {
    let f = Fixture::new();
    f.prepare();
    f.apply(&json!([
        {"op": "add", "comment": {"path": "src/lib.rs", "side": "new",
            "start": {"old": null, "new": 13}, "end": {"old": null, "new": 13},
            "anchor": {"before": ["line 11", "line 12"], "lines": ["thirteen"], "after": []},
            "text": "Needs a test.", "distance": 0}},
    ]));
    let plan = || {
        let run = f.run(&["post", PR, "--dry-run"]);
        run.ok();
        run.lines[0]["draft"].clone()
    };
    assert_eq!(plan().get("unsure"), None);

    f.move_head();
    f.reply("PullRequest", None, "pull_request_moved.json");
    f.run(&["prepare", PR]).ok();
    assert_eq!(plan().get("unsure"), None, "the session's head's are kept");

    f.down("files", Some(2));
    f.run(&["pull", PR]).ok();
    let planned = plan();
    assert_eq!(planned["plan"], "line");
    assert_eq!(planned["unsure"], true);
}

#[test]
fn prepare_prints_the_session_without_waiting_for_the_prs_files() {
    let f = Fixture::new();
    fs::write(f.path("stub/gh/files.hold"), "").expect("write");
    f.prepare();
    let rest = f.rest();
    assert_eq!(rest.len(), 1, "{rest:?}");
    let printed: usize = rest[0]
        .rsplit(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("a count");
    assert!(printed >= 1, "answered before the session line: {rest:?}");
}

/// The comments file the plugin kept itself before dffy kept them plays no
/// part in a request: the session doesn't name it, and dffy's file is the
/// one written. `dffy comments --session import` brings it in.
#[test]
fn the_comments_file_kept_before_is_left_for_an_import() {
    let f = Fixture::new();
    f.prepare();
    let old = f.kept("comments.json");
    fs::write(
        &old,
        r#"{"version":1,"comments":[{"id":"d1","path":"src/lib.rs","text":"Old draft","author":"","created":"2026-11-17T10:00:00Z"}]}"#,
    )
    .expect("write");
    f.reply("PullRequest", Some(3), "pull_request_page_1.json");
    f.reply("PullRequest", Some(4), "pull_request_page_2.json");
    let run = f.run(&["prepare", PR]);
    run.ok();
    assert_eq!(run.session().get("comments_from"), None);
    assert!(
        !f.comments()["comments"]
            .as_array()
            .expect("comments")
            .iter()
            .any(|c| c["text"] == "Old draft")
    );
    assert!(old.exists(), "left as it was");
}

/// The session and hunks say what you review, and are as private as your
/// drafts; the drafts themselves are dffy's to keep, not the plugin's.
#[test]
fn what_prepare_keeps_is_readable_by_you_alone() {
    let f = Fixture::new();
    f.prepare();
    assert!(!f.kept("comments.json").exists());
    for name in ["session.json", "hunks.json"] {
        let path = f.kept(name);
        let mode = fs::metadata(&path).expect("kept").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{name}");
    }
}

/// `pending` with a summary and nothing to post starts a pending review
/// to hold the summary, rather than dropping it.
#[test]
fn a_pending_summary_alone_starts_a_review_for_it() {
    let f = Fixture::new();
    f.prepare();
    f.reply("PostState", None, "post_state.json");
    f.reply("AddReview", None, "add_review.json");
    f.reply("UpdatePullRequestReview", None, "update_review.json");
    let before = f.operations().len();
    f.post("pending", "Looks close.\n").ok();
    assert_eq!(
        f.operations()[before..],
        ["PostState", "AddReview", "UpdatePullRequestReview"]
    );
    assert_eq!(
        f.requests().last().expect("the update")["variables"],
        json!({"review": "PRR_kwDOAAAAB84AAAAR", "body": "Looks close."})
    );
}

/// A post with a submitting event and no drafts left submits your pending
/// review with the summary, and what was pending in it no longer is.
#[test]
fn a_post_with_nothing_new_submits_the_pending_review() {
    let f = Fixture::new();
    f.prepare();
    let ids = f.add_drafts();
    f.reply("PostState", Some(1), "post_state.json");
    f.reply("PostState", Some(2), "post_state_pending.json");
    f.reply("AddReview", None, "add_review.json");
    f.reply("AddPullRequestReviewThread", Some(1), "add_thread_1.json");
    f.reply("AddPullRequestReviewThread", Some(2), "add_thread_2.json");
    f.reply("AddPullRequestReviewThreadReply", None, "add_reply.json");
    f.reply("SubmitPullRequestReview", None, "submit.json");
    f.post("pending", "").ok();
    assert_eq!(f.draft(&ids[2])["pending"], true);
    let before = f.operations().len();

    let run = f.post("approve", "Ship it\n");
    run.ok();
    assert_eq!(
        run.lines,
        [
            json!({"last_reviewed_head": f.head, "posted": 0, "failed": 0, "pending": 2,
            "url": format!("{URL}#pullrequestreview-2001")})
        ]
    );
    assert_eq!(
        f.operations()[before..],
        ["PostState", "SubmitPullRequestReview"]
    );
    assert_eq!(
        f.requests().last().expect("the submit")["variables"],
        json!({"review": "PRR_kwDOAAAAB84AAAAR", "event": "APPROVE", "body": "Ship it"})
    );
    assert!(f.draft(&ids[2]).get("pending").is_none(), "submitted");
}

/// `detect` answers for the branch checked out: from the session kept for
/// it without asking GitHub, else from one query; on another branch with
/// no PR, or a detached HEAD, with none. A partial clone's remote, which
/// git lists with its filter, is found as any other.
#[test]
fn detect_finds_the_pr_of_the_branch_from_its_session_else_by_one_query() {
    let f = Fixture::new();
    git(&f.work(), &["checkout", "-q", "-b", "five"]);
    git(&f.work(), &["config", "remote.origin.promisor", "true"]);
    git(
        &f.work(),
        &["config", "remote.origin.partialclonefilter", "blob:none"],
    );
    f.reply("Detect", None, "detect.json");
    let run = f.run(&["detect"]);
    run.ok();
    assert_eq!(
        run.lines[0]["review"],
        json!({"id": "7", "title": "Spell out five", "url": URL,
            "base": f.base, "head": f.head, "last_reviewed_head": null, "state": "open"})
    );
    assert_eq!(f.operations(), ["Detect"]);

    let prepared = f.prepare();
    let before = f.operations().len();
    let run = f.run(&["detect"]);
    run.ok();
    assert_eq!(run.lines, [json!({"review": prepared.session()})]);
    assert_eq!(f.operations().len(), before, "from the session");

    git(&f.work(), &["checkout", "-q", "--detach"]);
    assert_eq!(f.run(&["detect"]).lines, [json!({"review": null})]);
}

/// A session kept for a branch name answers only while the branch holds
/// its head: a branch of that name cut after the PR merged, squashed or
/// with a merge commit, is asked about.
#[test]
fn detect_asks_about_a_branch_cut_again_after_its_pr_merged() {
    let f = Fixture::new();
    f.prepare();
    f.reply("Detect", None, "detect.json");
    let work = f.work();
    let detected = |label: &str| {
        let before = f.operations().len();
        f.run(&["detect"]).ok();
        assert_eq!(f.operations()[before..], ["Detect"], "{label}");
    };
    fs::write(work.join("notes.txt"), "squashed\n").expect("write");
    git(&work, &["commit", "-q", "-am", "squashed"]);
    git(&work, &["checkout", "-q", "-b", "five"]);
    detected("squashed");

    git(&work, &["checkout", "-q", "main"]);
    git(&work, &["branch", "-q", "-D", "five"]);
    git(&work, &["merge", "-q", "--no-ff", "-m", "merged", &f.head]);
    git(&work, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(&work, &["checkout", "-q", "-b", "five"]);
    detected("merged");

    // Without the remote's default branch, any of its branches but the
    // PR's own that has the head says it merged.
    git(
        &work,
        &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
    );
    detected("merged, without origin/HEAD");
}

/// A PR rebased since it was prepared asks GitHub once: the head GitHub
/// names then is kept, and tells the branch by from then on, with the
/// remote's default branch or without it.
#[test]
fn detect_asks_once_about_a_branch_rebased_since_prepare() {
    let f = Fixture::new();
    f.prepare();
    let work = f.work();
    fs::write(work.join("notes.txt"), "upstream\n").expect("write");
    git(&work, &["commit", "-q", "-am", "upstream"]);
    git(&work, &["checkout", "-q", "-b", "five"]);
    fs::write(work.join("src/lib.rs"), "five, rebased\n").expect("write");
    git(&work, &["commit", "-q", "-am", "five, rebased"]);
    let rebased = git(&work, &["rev-parse", "HEAD"]);
    f.reply("Detect", None, "detect.json");
    let mut answer = f.answer("Detect.json");
    answer["data"]["repository"]["pullRequests"]["nodes"][1]["headRefOid"] = json!(rebased);
    fs::write(f.path("stub/gh/Detect.json"), answer.to_string()).expect("write");
    let asked = || {
        let before = f.operations().len();
        f.run(&["detect"]).ok();
        f.operations().len() - before
    };
    assert_eq!(asked(), 1);
    assert_eq!(asked(), 0, "by the head GitHub named");
    git(
        &work,
        &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
    );
    assert_eq!(asked(), 0, "without origin/HEAD");
}

/// `detect` has no review for a repository whose remote isn't on a GitHub
/// host, and asks GitHub nothing for it: a remote that names no host
/// isn't asked about at all, and another host only of gh's own logins,
/// once a day. A host gh is logged in to is GitHub's, and every API call
/// names it.
#[test]
fn detect_asks_nothing_of_a_host_that_isnt_githubs() {
    let f = Fixture::new();
    let work = f.work();
    git(&work, &["checkout", "-q", "-b", "five"]);
    let auth = || fs::read_to_string(f.path("stub/auth")).unwrap_or_default();
    let detect = || {
        let run = f.run(&["detect"]);
        run.ok();
        run.lines
    };

    let local = f.path("github/acme/widgets.git");
    git(
        &work,
        &[
            "remote",
            "set-url",
            "origin",
            local.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(detect(), [json!({"review": null})]);
    assert_eq!(auth(), "");

    git(
        &work,
        &[
            "remote",
            "set-url",
            "origin",
            "git@gitlab.com:acme/widgets.git",
        ],
    );
    assert_eq!(detect(), [json!({"review": null})]);
    assert_eq!(detect(), [json!({"review": null})]);
    assert_eq!(auth(), "auth status --hostname gitlab.com\n", "asked once");
    assert!(!f.path("stub/hostnames").exists(), "no API call");

    fs::write(f.path("stub/gh/hosts"), "ghe.example.com\n").expect("hosts");
    git(
        &work,
        &[
            "remote",
            "set-url",
            "origin",
            "git@ghe.example.com:acme/widgets.git",
        ],
    );
    f.reply("Detect", None, "detect.json");
    assert_eq!(detect()[0]["review"]["id"], "7");
    let hostnames = fs::read_to_string(f.path("stub/hostnames")).expect("API calls");
    assert_eq!(hostnames, "ghe.example.com\n");
}

/// GitHub having no PR for a branch is kept for 10 minutes at the
/// branch's commit, so `detect` asks again only once it moved.
#[test]
fn detect_keeps_that_a_branch_has_no_pr_until_it_moves() {
    let f = Fixture::new();
    let work = f.work();
    git(&work, &["checkout", "-q", "-b", "nothing-yet"]);
    let none = json!({"data": {"repository": {"pullRequests": {"nodes": []}}}});
    fs::write(f.path("stub/gh/Detect.json"), none.to_string()).expect("fixture");
    let detect = || {
        let run = f.run(&["detect"]);
        run.ok();
        assert_eq!(run.lines, [json!({"review": null})]);
        f.operations().len()
    };
    assert_eq!(detect(), 1);
    assert_eq!(detect(), 1, "kept");
    fs::write(work.join("notes.txt"), "moved on\n").expect("write");
    git(&work, &["commit", "-q", "-am", "moved on"]);
    assert_eq!(detect(), 2, "the branch moved");
    git(&work, &["checkout", "-q", "-b", "other"]);
    assert_eq!(detect(), 3, "another branch");
}

/// A fork's clone has the PR's repository as `upstream`: that's the one
/// asked for and fetched from, as gh takes it, unless `gh repo
/// set-default` picked another remote.
#[test]
fn the_repository_is_upstream_or_the_remote_gh_resolved() {
    let f = Fixture::new();
    let work = f.work();
    git(
        &work,
        &[
            "remote",
            "set-url",
            "origin",
            "git@github.com:forker/widgets.git",
        ],
    );
    git(
        &work,
        &[
            "remote",
            "add",
            "upstream",
            "git@github.com:acme/widgets.git",
        ],
    );
    f.log_git();
    f.prepare();
    let asked = &f.requests()[0]["variables"];
    assert_eq!(
        (&asked["owner"], &asked["name"]),
        (&json!("acme"), &json!("widgets"))
    );
    assert!(
        f.git_calls()
            .iter()
            .any(|c| c.starts_with("fetch --quiet --no-tags --end-of-options upstream ")),
        "{:?}",
        f.git_calls()
    );

    git(
        &work,
        &["remote", "add", "picked", "git@github.com:acme/gadgets.git"],
    );
    git(&work, &["config", "remote.picked.gh-resolved", "base"]);
    f.reply("List", None, "list.json");
    f.run(&["list"]).ok();
    let requests = f.requests();
    let asked = &requests.last().expect("List")["variables"];
    assert_eq!(
        (&asked["owner"], &asked["name"]),
        (&json!("acme"), &json!("gadgets"))
    );
}

#[test]
fn list_shows_prs_waiting_on_you_yours_and_the_current_branches_in_one_query() {
    let f = Fixture::new();
    f.reply("List", None, "list.json");
    let run = f.run(&["list"]);
    run.ok();
    let pr = |n: u64, title: &str, author: &str, head: &str, reason: &str| {
        json!({"id": n.to_string(), "title": title, "author": author,
            "url": format!("https://github.com/acme/widgets/pull/{n}"),
            "head_ref": head, "reason": reason})
    };
    // Not #12, a fork's from a branch of the same name.
    assert_eq!(
        run.lines,
        [json!({"reviews": [
            pr(7, "Spell out five", "octocat", "five", "current_branch"),
            pr(9, "Add a cache", "hubot", "cache", "review_requested"),
            pr(11, "Document the cache", "ada", "docs", "mine"),
        ]})]
    );
    let vars = &f.requests()[0]["variables"];
    assert_eq!(
        *vars,
        json!({"owner": "acme", "name": "widgets", "branch": "main", "onBranch": true,
            "number": 0, "byNumber": false,
            "requested": "is:pr is:open review-requested:@me repo:acme/widgets",
            "mine": "is:pr is:open author:@me repo:acme/widgets"})
    );

    // On a detached HEAD there's no branch to ask for, rather than every PR.
    git(&f.work(), &["checkout", "-q", "--detach"]);
    f.run(&["list"]).ok();
    assert_eq!(f.requests()[1]["variables"]["onBranch"], false);
}

#[test]
fn prefetch_fetches_the_heads_of_the_prs_list_shows() {
    let f = Fixture::new();
    let url = f.path("github/acme/widgets.git");
    git(&url, &["update-ref", "refs/pull/9/head", &f.head2]);
    git(&url, &["update-ref", "refs/pull/11/head", &f.base]);
    f.reply("List", None, "list.json");
    let run = f.run(&["prefetch"]);
    run.ok();
    assert_eq!(run.lines, [json!({"fetched": [7, 9, 11]})]);
    assert_eq!(git(&f.work(), &["rev-parse", "refs/dffy/pull/7"]), f.head);
    assert_eq!(git(&f.work(), &["rev-parse", "refs/dffy/pull/9"]), f.head2);
    assert_eq!(git(&f.work(), &["rev-parse", "refs/dffy/pull/11"]), f.base);
}

/// Methods it doesn't have, a request without its review's number, an
/// unknown event and a dffy speaking another protocol get JSON-RPC's
/// errors, before anything is read or sent; the session goes on. On the
/// command line, anything but its own commands is refused with exit 2.
#[test]
fn what_the_plugin_doesnt_speak_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dffy-review-github"));
    cmd.current_dir(dir.path()).env("PATH", "/nonexistent");
    let mut session = Session::start(&mut cmd, dir.path().join("out"));
    for (id, (method, params, code, says)) in [
        ("frobnicate", json!({}), -32601, "has no frobnicate"),
        ("check", json!({"id": PR}), -32601, "has no check"),
        (
            "prepare",
            json!({}),
            -32602,
            "isn't a pull request's number",
        ),
        (
            "pull",
            json!({"id": "seven"}),
            -32602,
            "isn't a pull request's number",
        ),
        (
            "post",
            json!({"id": PR, "event": "merge"}),
            -32602,
            "unknown event merge",
        ),
        (
            "initialize",
            json!({"protocol": 2}),
            -32602,
            "speaks review protocol 1, and dffy asked for 2",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let (notes, answer) = session.call(id as u64 + 1, method, &params);
        assert_eq!(notes, [] as [Value; 0], "{method}");
        let error = answer.expect_err(method);
        assert_eq!(error["code"], code, "{method}");
        let message = error["message"].as_str().expect("words");
        assert!(message.contains(says), "{method}: {message}");
    }
    session.close();

    for args in [
        &["frobnicate"][..],
        &["prepare", PR],
        &["prune", "--days", "x"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_dffy-review-github"))
            .args(args)
            .env("PATH", "/nonexistent")
            .stdin(Stdio::null())
            .output()
            .expect("runs");
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}

/// Requests run side by side: a dry run answers while a `prepare` waits on
/// GitHub. Cancelled, the `prepare` stops before its next page, writing
/// none of it and removing nothing, and says it was cancelled.
#[test]
fn a_dry_run_answers_beside_a_prepare_which_stops_when_cancelled() {
    let f = Fixture::new();
    f.prepare();
    let before = f.comments()["comments"].clone();
    let mut page = f.answer("PullRequest.2.json");
    page["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]["nodes"]
        [0]["body"] = json!("Changed meanwhile.");
    fs::write(f.path("stub/gh/PullRequest.4.json"), page.to_string()).expect("fixture");
    f.reply("PullRequest", Some(3), "pull_request_page_1.json");
    let gate = f.path("stub/gh/PullRequest.4.gate");
    fs::write(&gate, "").expect("gate");

    let mut session = f.session(&mut f.command(&f.stub_path()));
    let file = carried(&f.comments_file());
    session.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "prepare",
        "params": {"id": PR, "known": carry::known(&file)}}));
    assert_eq!(session.said()["method"], "session");
    assert_eq!(session.said()["params"]["progress"], "1/4");
    let dry_run =
        json!({"id": PR, "drafts": carry::drafts(&file), "resolves": carry::resolves(&file)});
    let (_, planned) = session.call(2, "dryRun", &dry_run);
    assert!(planned.is_ok(), "{planned:?}");
    session.notify("$/cancelRequest", &json!({"id": 1}));
    // Asked for before the cancel came.
    let start = Instant::now();
    while f.events().len() < 4 {
        assert!(start.elapsed() < WAIT, "{:?}", f.events());
        std::thread::sleep(Duration::from_millis(5));
    }
    fs::remove_file(&gate).expect("open the gate");
    let answer = session.said();
    assert_eq!(answer["id"], 1);
    assert_eq!(answer["error"]["code"], -32800, "{answer}");
    session.close();
    assert_eq!(f.comments()["comments"], before);
}

#[test]
fn version_prints_the_release_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_dffy-review-github"))
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .expect("runs");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("dffy-review-github {}\n", env!("CARGO_PKG_VERSION"))
    );
}

/// Only your comments are drafts: one without a `ref` by someone else, an
/// agent, would go out under your name, so neither the plan nor the post
/// counts it, and it stays as it was.
#[test]
fn a_comment_by_someone_else_is_never_posted() {
    let f = Fixture::new();
    f.prepare();
    let draft = |author: &str, text: &str| {
        json!({"op": "add", "comment": {"path": "src/lib.rs", "side": "new",
            "start": {"old": null, "new": 13}, "end": {"old": null, "new": 13},
            "anchor": {"before": ["line 11", "line 12"], "lines": ["thirteen"], "after": []},
            "text": text, "author": author, "distance": 0}})
    };
    let answer = f.apply(&json!([
        draft("", "Needs a test."),
        draft("claude", "Off by one?")
    ]));
    let (yours, theirs) = (answer["ids"][0].clone(), answer["ids"][1].clone());

    let run = f.run(&["post", PR, "--dry-run"]);
    run.ok();
    assert_eq!(
        run.lines,
        [
            json!({"draft": {"id": yours, "path": "src/lib.rs", "plan": "line", "line": 13, "side": "new"}}),
        ]
    );

    f.reply("PostState", None, "post_state.json");
    f.reply("AddReview", None, "add_review.json");
    f.reply("AddPullRequestReviewThread", None, "add_thread_1.json");
    let run = f.post("pending", "");
    run.ok();
    assert_eq!(
        (&run.last()["posted"], &run.last()["failed"]),
        (&json!(1), &json!(0))
    );
    let posted = f.draft(yours.as_str().expect("an id"));
    assert!(posted.get("ref").is_some());
    assert_eq!(posted["pending"], true, "left pending as asked");
    assert!(
        f.draft(theirs.as_str().expect("an id"))
            .get("ref")
            .is_none()
    );
    assert_eq!(
        f.operations()
            .iter()
            .filter(|o| *o == "AddPullRequestReviewThread")
            .count(),
        1
    );
}

/// Where GitHub's hunks of a file aren't kept, git's decide, drawn as
/// GitHub draws them: changes with seven unchanged lines between, two
/// hunks of three lines of context one line apart, are one hunk, so a
/// draft from the first to the second goes on its lines; with eight
/// between, they're two, and it becomes a file comment.
#[test]
fn gits_hunks_join_as_githubs_do() {
    for (gap, plan) in [(7, "line"), (8, "file")] {
        let mut f = Fixture::new();
        let author = f.path("author");
        let second = 5 + gap + 1;
        let changed = numbered((1..=40).map(|i| match i {
            5 => "five".to_owned(),
            i if i == second => "changed".to_owned(),
            i => format!("long {i}"),
        }));
        fs::write(author.join("src/long.rs"), changed).expect("write");
        git(&author, &["commit", "-q", "-am", "two changes"]);
        f.head = git(&author, &["rev-parse", "HEAD"]);
        let url = f.path("github/acme/widgets.git");
        let url = url.to_str().expect("utf-8 path");
        git(
            &author,
            &[
                "push",
                "-q",
                "-f",
                url,
                &format!("{}:refs/pull/{PR}/head", f.head),
            ],
        );
        f.reply("PullRequest", Some(1), "pull_request_page_1.json");
        f.reply("PullRequest", Some(2), "pull_request_page_2.json");
        f.down("files", None);
        f.run(&["prepare", PR]).ok();
        let lines: Vec<String> = (5..=second)
            .map(|i| match i {
                5 => "five".to_owned(),
                i if i == second => "changed".to_owned(),
                i => format!("long {i}"),
            })
            .collect();
        let answer = f.apply(&json!([
            {"op": "add", "comment": {"path": "src/long.rs", "side": "new",
                "start": {"old": 5, "new": 5}, "end": {"old": second, "new": second},
                "anchor": {"before": ["long 3", "long 4"], "lines": lines,
                    "after": [format!("long {}", second + 1), format!("long {}", second + 2)]},
                "text": "Both?", "distance": 0}},
        ]));
        let run = f.run(&["post", PR, "--dry-run"]);
        run.ok();
        let planned = run
            .lines
            .iter()
            .find(|l| l["draft"]["id"] == answer["ids"][0])
            .unwrap_or_else(|| panic!("{gap}: {:?}", run.lines));
        assert_eq!(planned["draft"]["plan"], plan, "{gap}: {planned}");
        assert_eq!(planned["draft"]["unsure"], true, "{gap}: {planned}");
    }
}

/// `detect` passes over a PR from another repository's branch of the same
/// name, and over a kept session of a fork's PR, or of one that was no
/// longer open when `prepare` last asked: those ask GitHub. A session kept
/// before the plugin recorded either is taken as open and origin's own.
#[test]
fn detect_passes_over_forks_and_prs_no_longer_open() {
    let f = Fixture::new();
    git(&f.work(), &["checkout", "-q", "-b", "five"]);
    let prepared = f.prepare();
    let session = f.saved_session();
    assert_eq!(
        (&session["state"], &session["cross_repository"]),
        (&json!("OPEN"), &json!(false))
    );
    let mut merged = f.answer("PullRequest.1.json");
    merged["data"]["repository"]["pullRequest"]["state"] = json!("MERGED");
    fs::write(f.path("stub/gh/PullRequest.3.json"), merged.to_string()).expect("fixture");
    f.reply("PullRequest", Some(4), "pull_request_page_2.json");
    f.run(&["prepare", PR]).ok();
    assert_eq!(f.saved_session()["state"], "MERGED");
    assert_eq!(f.saved_session()["head"], session["head"]);

    let forks = json!({"data": {"repository": {"pullRequests": {"nodes": [
        {"number": 8, "title": "A fork's five", "url": URL, "baseRefOid": f.base,
            "headRefOid": f.head2, "isCrossRepository": true, "state": "OPEN"}]}}}});
    fs::write(f.path("stub/gh/Detect.json"), forks.to_string()).expect("fixture");
    let detect = || {
        let before = f.operations().len();
        let run = f.run(&["detect"]);
        run.ok();
        (run.lines, f.operations().len() - before)
    };
    assert_eq!(detect(), (vec![json!({"review": null})], 1));
    assert_eq!(
        f.requests().last().expect("Detect")["variables"]["byNumber"],
        false
    );

    let mut fork = session.clone();
    fork["cross_repository"] = json!(true);
    f.save_session(&fork);
    // That GitHub had none for the branch was kept.
    assert_eq!(detect(), (vec![json!({"review": null})], 0));
    fs::remove_file(f.kept("../detect.json")).expect("kept");
    assert_eq!(detect(), (vec![json!({"review": null})], 1));

    let mut old = session;
    for field in ["state", "cross_repository"] {
        old.as_object_mut().expect("a session").remove(field);
    }
    f.save_session(&old);
    let mut stateless = prepared.session().clone();
    stateless
        .as_object_mut()
        .expect("a session")
        .remove("state");
    assert_eq!(detect(), (vec![json!({ "review": stateless })], 0));
}

/// A branch `gh pr checkout` made tracks the PR's own ref, which names the
/// PR, a fork's too, whatever the branch is called: from its session, else
/// asked for by number, and none once it isn't open.
#[test]
fn detect_takes_the_pr_a_branch_was_checked_out_from() {
    let f = Fixture::new();
    git(&f.work(), &["checkout", "-q", "-b", "fork-main"]);
    git(
        &f.work(),
        &["config", "branch.fork-main.merge", "refs/pull/7/head"],
    );
    let found = |state: &str| {
        json!({"data": {"repository": {"pullRequest": {"number": 7, "title": "Spell out five",
            "url": URL, "baseRefOid": f.base, "headRefOid": f.head,
            "isCrossRepository": true, "state": state}}}})
        .to_string()
    };
    fs::write(f.path("stub/gh/Detect.1.json"), found("OPEN")).expect("fixture");
    let run = f.run(&["detect"]);
    run.ok();
    assert_eq!(run.lines[0]["review"]["id"], "7");
    assert_eq!(
        f.requests().last().expect("Detect")["variables"],
        json!({"owner": "acme", "name": "widgets", "branch": "", "number": 7, "byNumber": true})
    );

    let prepared = f.prepare();
    let mut session = f.saved_session();
    session["cross_repository"] = json!(true);
    f.save_session(&session);
    let before = f.operations().len();
    assert_eq!(
        f.run(&["detect"]).lines,
        [json!({"review": prepared.session()})]
    );
    assert_eq!(f.operations().len(), before, "from the session");

    session["state"] = json!("CLOSED");
    f.save_session(&session);
    fs::write(f.path("stub/gh/Detect.2.json"), found("CLOSED")).expect("fixture");
    assert_eq!(f.run(&["detect"]).lines, [json!({"review": null})]);
    assert_eq!(f.operations().len(), before + 1);
}

/// A fork's PR that `gh pr checkout` can push to has the branch track the
/// fork's branch, by the fork's URL: the PR is the one from that branch of
/// the fork's owner, not origin's own of the same name, from GitHub, then
/// from its session.
#[test]
fn detect_takes_a_forks_pr_by_the_fork_branch_its_branch_tracks() {
    let f = Fixture::new();
    let work = f.work();
    git(&work, &["checkout", "-q", "-b", "forker-five"]);
    let fork = "https://github.com/forker/widgets.git";
    git(&work, &["config", "branch.forker-five.remote", fork]);
    git(
        &work,
        &["config", "branch.forker-five.merge", "refs/heads/five"],
    );
    let pr = |n: u64, cross: bool, owner: &str| {
        json!({"number": n, "title": "Spell out five", "url": URL, "baseRefOid": f.base,
            "headRefOid": f.head, "isCrossRepository": cross,
            "headRepositoryOwner": {"login": owner}, "state": "OPEN"})
    };
    let found = json!({"data": {"repository": {"pullRequests": {"nodes":
        [pr(8, false, "acme"), pr(7, true, "Forker")]}}}});
    fs::write(f.path("stub/gh/Detect.json"), found.to_string()).expect("fixture");
    let run = f.run(&["detect"]);
    run.ok();
    assert_eq!(run.lines[0]["review"]["id"], "7");
    assert_eq!(
        f.requests().last().expect("Detect")["variables"]["branch"],
        "five"
    );

    f.reply("PullRequest", Some(1), "pull_request_page_1.json");
    let mut first = f.answer("PullRequest.1.json");
    let about = &mut first["data"]["repository"]["pullRequest"];
    about["isCrossRepository"] = json!(true);
    about["headRepositoryOwner"] = json!({"login": "forker"});
    fs::write(f.path("stub/gh/PullRequest.1.json"), first.to_string()).expect("fixture");
    f.reply("PullRequest", Some(2), "pull_request_page_2.json");
    f.reply("files", None, "files.json");
    let prepared = f.run(&["prepare", PR]);
    prepared.ok();
    assert_eq!(f.saved_session()["head_owner"], "forker");
    let before = f.operations().len();
    assert_eq!(
        f.run(&["detect"]).lines,
        [json!({"review": prepared.session()})]
    );
    assert_eq!(f.operations().len(), before, "from the session");
}

/// When the PR's branch is the fork's default one, `gh pr checkout` names
/// the local branch `<owner>/<branch>`; with a remote for the fork there
/// already, the branch tracks that remote's. Either way it's the fork's PR.
#[test]
fn detect_takes_a_forks_default_branch_pr_through_its_remote() {
    let f = Fixture::new();
    let work = f.work();
    git(
        &work,
        &[
            "remote",
            "add",
            "forker",
            "git@github.com:forker/widgets.git",
        ],
    );
    git(&work, &["checkout", "-q", "-b", "forker/main"]);
    git(&work, &["config", "branch.forker/main.remote", "forker"]);
    git(
        &work,
        &["config", "branch.forker/main.merge", "refs/heads/main"],
    );
    let found = json!({"data": {"repository": {"pullRequests": {"nodes": [
        {"number": 7, "title": "Spell out five", "url": URL, "baseRefOid": f.base,
            "headRefOid": f.head, "isCrossRepository": true,
            "headRepositoryOwner": {"login": "forker"}, "state": "OPEN"}]}}}});
    fs::write(f.path("stub/gh/Detect.json"), found.to_string()).expect("fixture");
    let run = f.run(&["detect"]);
    run.ok();
    assert_eq!(run.lines[0]["review"]["id"], "7");
    let vars = &f.requests()[0]["variables"];
    assert_eq!(
        (&vars["branch"], &vars["byNumber"]),
        (&json!("main"), &json!(false))
    );
}

/// dffy names itself in `DFFY` when it runs the plugin, and that dffy
/// writes the comments, not whichever `dffy` comes first on PATH.
#[test]
fn the_dffy_that_runs_the_plugin_writes_the_comments() {
    let f = Fixture::new();
    let decoy = f.path("stub/bin/dffy");
    fs::remove_file(&decoy).expect("remove");
    fs::write(&decoy, include_str!("fixtures/stubs/decoy-dffy.sh")).expect("decoy");
    fs::set_permissions(&decoy, fs::Permissions::from_mode(0o755)).expect("chmod");
    f.reply("PullRequest", Some(1), "pull_request_page_1.json");
    f.reply("PullRequest", Some(2), "pull_request_page_2.json");
    f.reply("files", None, "files.json");
    f.run_as_dffy(&["prepare", PR]).ok();
    assert_eq!(
        f.comment("github:PRRC_kwDOAAAAB84AAAB1")["text"],
        "Spell it out."
    );
}

/// A range that starts on the other side is anchored by its last line
/// alone, so when that line is elsewhere at the head, it's posted on that
/// line alone.
#[test]
fn a_range_from_the_other_side_whose_last_line_moved_posts_on_that_line() {
    let f = Fixture::new();
    f.prepare();
    f.apply(&json!([
        {"op": "add", "comment": {"path": "src/lib.rs", "side": "new", "start_side": "old",
            "start": {"old": 11, "new": null}, "end": {"old": null, "new": 15},
            "anchor": {"before": ["line 11", "line 12"], "lines": ["thirteen"], "after": []},
            "text": "Hm.", "distance": 0}},
    ]));
    f.reply("PostState", None, "post_state.json");
    f.reply("AddReview", None, "add_review.json");
    f.reply("AddPullRequestReviewThread", None, "add_thread_1.json");
    f.post("pending", "").ok();
    let thread = f
        .requests()
        .into_iter()
        .find(|r| r["operationName"] == "AddPullRequestReviewThread")
        .expect("a thread");
    assert_eq!(
        thread["variables"],
        json!({"review": "PRR_kwDOAAAAB84AAAAR", "path": "src/lib.rs", "body": "Hm.",
            "line": 13, "side": "RIGHT"})
    );
}

/// A thread's comments past its first page are asked for a page at a time
/// before the thread is written, and are still there after the next
/// import; when one of those pages fails, nothing is removed.
#[test]
fn a_threads_comments_past_its_first_page_are_imported() {
    let f = Fixture::new();
    f.reply("PullRequest", Some(1), "pull_request_page_1.json");
    let mut first = f.answer("PullRequest.1.json");
    first["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]["pageInfo"] =
        json!({"hasNextPage": true, "endCursor": "Y3Vyc29yOjI="});
    for n in [1, 3, 5] {
        fs::write(
            f.path(&format!("stub/gh/PullRequest.{n}.json")),
            first.to_string(),
        )
        .expect("fixture");
    }
    for n in [2, 4, 6] {
        f.reply("PullRequest", Some(n), "pull_request_page_2.json");
    }
    f.reply("files", None, "files.json");
    let more = json!({"data": {"node": {"comments": {
        "pageInfo": {"hasNextPage": false, "endCursor": "Y3Vyc29yOjM="},
        "nodes": [{"id": "PRRC_more", "url": URL, "body": "One more.",
            "createdAt": "2026-11-16T13:00:00Z", "viewerDidAuthor": false,
            "author": {"login": "hubot"}, "originalCommit": {"oid": f.head}}]}}}});
    fs::write(f.path("stub/gh/ThreadComments.json"), more.to_string()).expect("fixture");

    f.run(&["prepare", PR]).ok();
    let root = f.comment("github:PRRC_kwDOAAAAB84AAAB1");
    let reply = f.comment("github:PRRC_more");
    assert_eq!(reply["in_reply_to"], root["id"]);
    assert_eq!(reply["text"], "One more.");
    let asked: Vec<Value> = f
        .requests()
        .into_iter()
        .filter(|r| r["operationName"] == "ThreadComments")
        .map(|r| r["variables"].clone())
        .collect();
    assert_eq!(
        asked,
        [json!({"thread": "PRRT_kwDOAAAAB84AAAA1", "after": "Y3Vyc29yOjI="})]
    );

    f.run(&["prepare", PR]).ok();
    assert_eq!(f.comment("github:PRRC_more"), reply);

    let before = f.comments()["comments"].clone();
    f.down("ThreadComments", Some(3));
    assert_eq!(f.run(&["prepare", PR]).code, Some(1));
    assert_eq!(f.comments()["comments"], before);
}

/// Once an import has read every page, what GitHub no longer has is
/// removed, someone else's or yours, deleted or in a pending review
/// discarded. A first comment whose thread holds a reply of yours not
/// posted yet stays, for the reply, no longer posted. Your pending review's
/// comments are still there.
#[test]
fn an_import_removes_what_github_dropped_but_keeps_a_thread_for_your_drafts() {
    let f = Fixture::new();
    f.prepare();
    let file_thread = f.comment("github:PRRC_kwDOAAAAB84AAAB5");
    let posted = |n: &str, text: &str| {
        json!({"op": "upsert", "comment": {"ref": format!("github:PRRC_{n}"), "path": "src/lib.rs",
            "text": text, "author": "", "url": URL, "github_thread": format!("PRRT_{n}"),
            "host_resolved": false}})
    };
    let answer = f.apply(&json!([
        {"op": "add", "comment": {"in_reply_to_ref": "github:PRRC_kwDOAAAAB84AAAB5", "text": "Adding one."}},
        posted("discarded", "Mine."),
        posted("pending", "Pending."),
    ]));
    let id = |i: usize| answer["ids"][i].clone();

    let mut page = f.answer("PullRequest.1.json");
    let pr = &mut page["data"]["repository"]["pullRequest"];
    pr["reviewThreads"]["pageInfo"]["hasNextPage"] = json!(false);
    pr["reviewThreads"]["totalCount"] = json!(1);
    pr["reviewThreads"]["nodes"][0]["comments"]["nodes"]
        .as_array_mut()
        .expect("comments")
        .truncate(1);
    pr["reviews"] = json!({"nodes": [{"comments": {"pageInfo": {"hasNextPage": false},
        "nodes": [{"id": "PRRC_pending"}]}}]});
    fs::write(f.path("stub/gh/PullRequest.3.json"), page.to_string()).expect("fixture");
    f.run(&["prepare", PR]).ok();

    let refs: Vec<Value> = f.comments()["comments"]
        .as_array()
        .expect("comments")
        .iter()
        .map(|c| c["ref"].clone())
        .collect();
    assert_eq!(
        refs,
        [
            json!("github:PRRC_kwDOAAAAB84AAAB1"),
            Value::Null,
            Value::Null,
            json!("github:PRRC_pending"),
        ]
    );
    let kept = f.draft(file_thread["id"].as_str().expect("an id"));
    for field in ["ref", "url", "github_thread", "host_resolved"] {
        assert!(kept.get(field).is_none(), "{field}: {kept}");
    }
    assert_eq!(kept["author"], "octocat");
    assert_eq!(
        f.draft(id(0).as_str().expect("an id"))["in_reply_to"],
        file_thread["id"]
    );

    let run = f.run(&["post", PR, "--dry-run"]);
    run.ok();
    assert_eq!(
        run.lines,
        [
            json!({"draft": {"id": id(0), "path": "notes.txt", "plan": "skip",
                "reason": "its thread isn't posted"}}),
        ]
    );
}

/// An import that didn't read every thread removes nothing, whether a page
/// failed or GitHub listed fewer threads than it counted: what it didn't
/// read isn't gone.
#[test]
fn an_import_that_didnt_read_every_thread_removes_nothing() {
    let f = Fixture::new();
    f.prepare();
    let before = f.comments()["comments"].clone();
    f.reply("PullRequest", Some(3), "pull_request_page_1.json");
    f.down("PullRequest", Some(4));
    assert_eq!(f.run(&["prepare", PR]).code, Some(1));
    assert_eq!(f.comments()["comments"], before);

    let mut short = f.answer("PullRequest.3.json");
    short["data"]["repository"]["pullRequest"]["reviewThreads"]["pageInfo"]["hasNextPage"] =
        json!(false);
    fs::write(f.path("stub/gh/PullRequest.5.json"), short.to_string()).expect("fixture");
    f.run(&["prepare", PR]).ok();
    assert_eq!(f.comments()["comments"], before);
}

/// A comment `post` writes while an import's first page is on its way
/// isn't in that page, and isn't taken for gone.
#[test]
fn a_comment_posted_during_an_import_stays_posted() {
    let f = Fixture::new();
    f.prepare();
    let posted = json!([{"op": "upsert", "comment": {"ref": "github:PRRC_racing",
        "path": "src/lib.rs", "text": "Posted meanwhile.", "author": "",
        "github_thread": "PRRT_racing", "host_resolved": false}}]);
    fs::write(
        f.path("stub/gh/PullRequest.3.apply.json"),
        posted.to_string(),
    )
    .expect("hook");
    f.reply("PullRequest", Some(3), "pull_request_page_1.json");
    f.reply("PullRequest", Some(4), "pull_request_page_2.json");
    let mut cmd = f.command(&f.stub_path());
    f.finish(
        cmd.env("STUB_COMMENTS", f.comments_file()),
        &["prepare", PR],
    )
    .ok();
    assert_eq!(f.comment("github:PRRC_racing")["text"], "Posted meanwhile.");
}

/// A conversation comment GitHub no longer has stays while you have a
/// draft reply under it, posted no longer, so the draft isn't lost; the
/// draft isn't posted, as its thread isn't on GitHub, and the next pull
/// goes as well as ever.
#[test]
fn a_general_comment_gone_from_github_stays_for_its_draft_replies() {
    let f = Fixture::new();
    f.reply("PullRequest", Some(1), "pull_request_conversation_1.json");
    f.reply("PullRequest", Some(2), "pull_request_conversation_2.json");
    f.reply("files", None, "files.json");
    f.run(&["prepare", PR]).ok();
    f.apply(&json!([{"op": "add", "comment":
        {"in_reply_to_ref": "github:IC_kwDOAAAAB84AAAC1", "text": "Agreed."}}]));

    let mut first = f.answer("PullRequest.1.json");
    first["data"]["repository"]["pullRequest"]["conversation"]["nodes"] = json!([]);
    fs::write(f.path("stub/gh/PullRequest.3.json"), first.to_string()).expect("fixture");
    f.reply("PullRequest", Some(4), "pull_request_conversation_2.json");
    f.run(&["pull", PR]).ok();
    let comments = f.comments()["comments"].clone();
    let comments = comments.as_array().expect("comments");
    assert!(
        !comments
            .iter()
            .any(|c| c["ref"] == "github:IC_kwDOAAAAB84AAAC1"),
        "{comments:?}"
    );
    let reply = comments
        .iter()
        .find(|c| c["text"] == "Agreed.")
        .expect("the draft stays");
    let root = comments
        .iter()
        .find(|c| c["id"] == reply["in_reply_to"])
        .expect("with what it answers");
    assert!(
        root.get("ref").is_none() && root.get("path").is_none(),
        "{root}"
    );

    let run = f.run(&["post", PR, "--dry-run"]);
    run.ok();
    let planned: Vec<&Value> = run.lines.iter().map(|l| &l["draft"]).collect();
    assert!(
        planned
            .iter()
            .any(|d| d["id"] == reply["id"] && d["reason"] == "its thread isn't posted"),
        "{planned:?}"
    );
    f.reply("PullRequest", Some(5), "pull_request_conversation_1.json");
    f.reply("PullRequest", Some(6), "pull_request_conversation_2.json");
    f.run(&["pull", PR]).ok();
}

/// The comments without a path: general comments.
fn general(f: &Fixture) -> Vec<Value> {
    f.comments()["comments"]
        .as_array()
        .expect("comments")
        .iter()
        .filter(|c| c.get("path").is_none())
        .cloned()
        .collect()
}

/// The PR's description, the comments in its conversation and the
/// summaries of its reviews come in as general comments: no path, a
/// `kind`, a review's state as its `label`, each page's by when they were
/// written. A review submitted without a summary is left out. The
/// conversation's next page is asked for alone, and progress counts
/// threads only. Pulling again changes nothing; what GitHub no longer has
/// goes.
#[test]
fn the_conversation_comes_in_as_general_comments() {
    let f = Fixture::new();
    f.reply("PullRequest", Some(1), "pull_request_conversation_1.json");
    f.reply("PullRequest", Some(2), "pull_request_conversation_2.json");
    f.reply("files", None, "files.json");
    let run = f.run(&["prepare", PR]);
    run.ok();
    assert_eq!(run.lines[1..], [json!({"progress": "0/0"})]);
    let requests = f.requests();
    let asked: Vec<&Value> = requests
        .iter()
        .filter(|r| r["operationName"] == "PullRequest")
        .map(|r| &r["variables"])
        .collect();
    assert_eq!(asked.len(), 2);
    assert_eq!(
        [
            &asked[0]["threads"],
            &asked[0]["comments"],
            &asked[0]["reviews"]
        ],
        [true, true, true]
    );
    let next = asked[1];
    assert_eq!(
        json!([
            next["threads"],
            next["comments"],
            next["reviews"],
            next["commentsAfter"]
        ]),
        json!([false, true, false, "Y3Vyc29yOnYyOpHOAAC7uQ"])
    );

    let imported = general(&f);
    let summary: Vec<Value> = imported
        .iter()
        .map(|c| json!([c["ref"], c["kind"], c["author"], c["created"], c["label"]]))
        .collect();
    assert_eq!(
        summary,
        [
            json!([
                "github:PR_kwDOAAAAB84AAAAH",
                "description",
                "octocat",
                "2026-11-15T08:00:00Z",
                null
            ]),
            json!([
                "github:IC_kwDOAAAAB84AAAC1",
                "comment",
                "hubot",
                "2026-11-16T08:00:00Z",
                null
            ]),
            json!([
                "github:PRR_kwDOAAAAB84AAAA3",
                "review",
                "",
                "2026-11-16T12:00:00Z",
                "changes requested"
            ]),
            json!([
                "github:PRR_kwDOAAAAB84AAAA1",
                "review",
                "hubot",
                "2026-11-17T09:00:00Z",
                "approved"
            ]),
            json!([
                "github:IC_kwDOAAAAB84AAAC2",
                "comment",
                "",
                "2026-11-16T09:00:00Z",
                null
            ]),
        ]
    );
    let description = &imported[0];
    assert_eq!(
        description["text"],
        "Spells out five.\n\nThe count was off by one."
    );
    assert_eq!(description["url"], URL);
    assert!(description.get("start").is_none() && description.get("anchor").is_none());

    f.reply("PullRequest", Some(3), "pull_request_conversation_1.json");
    f.reply("PullRequest", Some(4), "pull_request_conversation_2.json");
    f.run(&["pull", PR]).ok();
    assert_eq!(general(&f), imported, "the same comments, ids and all");

    // The description emptied, and a comment deleted.
    let mut first = f.answer("PullRequest.1.json");
    let pr = &mut first["data"]["repository"]["pullRequest"];
    pr["body"] = json!("");
    pr["conversation"]["nodes"] = json!([]);
    fs::write(f.path("stub/gh/PullRequest.5.json"), first.to_string()).expect("fixture");
    f.reply("PullRequest", Some(6), "pull_request_conversation_2.json");
    f.run(&["pull", PR]).ok();
    let left: Vec<&Value> = imported.iter().map(|c| &c["ref"]).collect();
    let now = general(&f);
    let now: Vec<&Value> = now.iter().map(|c| &c["ref"]).collect();
    assert_eq!(now, left[2..]);
}

/// A comment in your pending review on GitHub comes in `pending`, which
/// goes once GitHub has it submitted.
#[test]
fn comments_in_your_pending_review_come_in_pending() {
    let f = Fixture::new();
    f.reply("PullRequest", Some(1), "pull_request_page_1.json");
    let mut first = f.answer("PullRequest.1.json");
    first["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]["nodes"]
        [1]["state"] = json!("PENDING");
    fs::write(f.path("stub/gh/PullRequest.1.json"), first.to_string()).expect("fixture");
    f.reply("PullRequest", Some(2), "pull_request_page_2.json");
    f.reply("files", None, "files.json");
    f.run(&["prepare", PR]).ok();
    assert_eq!(f.comment("github:PRRC_kwDOAAAAB84AAAB2")["pending"], true);
    assert!(
        f.comment("github:PRRC_kwDOAAAAB84AAAB1")
            .get("pending")
            .is_none()
    );

    f.reply("PullRequest", Some(3), "pull_request_page_1.json");
    f.reply("PullRequest", Some(4), "pull_request_page_2.json");
    f.run(&["prepare", PR]).ok();
    assert!(
        f.comment("github:PRRC_kwDOAAAAB84AAAB2")
            .get("pending")
            .is_none()
    );
}

/// Your pending review's comments past its first page are read too, so an
/// import still removes what GitHub dropped, and not them.
#[test]
fn an_import_reads_every_page_of_your_pending_review() {
    let f = Fixture::new();
    f.prepare();
    let posted = |n: &str| {
        json!({"op": "upsert", "comment": {"ref": format!("github:PRRC_{n}"), "path": "src/lib.rs",
            "text": "Mine.", "author": "", "url": URL, "github_thread": format!("PRRT_{n}"),
            "host_resolved": false}})
    };
    f.apply(&json!([posted("later"), posted("dropped")]));
    f.reply("PullRequest", Some(3), "pull_request_page_1.json");
    let mut page = f.answer("PullRequest.3.json");
    page["data"]["repository"]["pullRequest"]["reviews"] = json!({"nodes": [{"comments": {
        "pageInfo": {"hasNextPage": true, "endCursor": "cGVuZGluZw=="},
        "nodes": [{"id": "PRRC_first"}]}}]});
    fs::write(f.path("stub/gh/PullRequest.3.json"), page.to_string()).expect("fixture");
    f.reply("PullRequest", Some(4), "pull_request_page_2.json");
    let rest = json!({"data": {"repository": {"pullRequest": {"reviews": {"nodes": [{"comments": {
        "pageInfo": {"hasNextPage": false, "endCursor": null},
        "nodes": [{"id": "PRRC_later"}]}}]}}}}});
    fs::write(f.path("stub/gh/PendingComments.json"), rest.to_string()).expect("fixture");
    f.run(&["prepare", PR]).ok();

    let refs: Vec<Value> = f.comments()["comments"]
        .as_array()
        .expect("comments")
        .iter()
        .map(|c| c["ref"].clone())
        .collect();
    assert!(refs.contains(&json!("github:PRRC_later")), "{refs:?}");
    assert!(!refs.contains(&json!("github:PRRC_dropped")), "{refs:?}");
    let asked: Vec<Value> = f
        .requests()
        .into_iter()
        .filter(|r| r["operationName"] == "PendingComments")
        .map(|r| r["variables"]["after"].clone())
        .collect();
    assert_eq!(asked, [json!("cGVuZGluZw==")]);
}

/// A post holds `post.lock` in the PR's directory: a second one while it
/// runs fails at once and sends nothing.
#[test]
fn a_second_post_while_one_runs_fails_and_sends_nothing() {
    let f = Fixture::new();
    f.prepare();
    f.add_drafts();
    let lock = fs::File::create(f.kept("post.lock")).expect("lock file");
    lock.lock().expect("locked");
    let before = (f.operations().len(), f.comments());
    let run = f.post("comment", "");
    assert_eq!(run.code, Some(1));
    assert!(run.error().contains("already posting"), "{}", run.error());
    assert_eq!((f.operations().len(), f.comments()), before);

    drop(lock);
    f.reply("PostState", None, "post_state.json");
    f.reply("AddReview", None, "add_review.json");
    f.reply("AddPullRequestReviewThread", Some(1), "add_thread_1.json");
    f.reply("AddPullRequestReviewThread", Some(2), "add_thread_2.json");
    f.reply("AddPullRequestReviewThreadReply", None, "add_reply.json");
    f.reply("SubmitPullRequestReview", None, "submit.json");
    f.post("comment", "").ok();
}

/// Makes the PR's session look last prepared `days` ago.
fn age_session(f: &Fixture, days: u64) {
    fs::File::options()
        .write(true)
        .open(f.kept("session.json"))
        .and_then(|s| s.set_modified(SystemTime::now() - Duration::from_secs(days * 86_400)))
        .expect("aged");
}

fn has_pull_ref(f: &Fixture) -> bool {
    Command::new("git")
        .current_dir(f.work())
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/dffy/pull/{PR}"),
        ])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `prune` drops what it keeps of a PR GitHub last said was merged or
/// closed, untouched for the days given (30 without), with its fetched
/// ref; the PR's comments are dffy's, and stay. The comments file it kept
/// itself before dffy kept them stays, with the PR's session, while it
/// holds a comment without a ref or can't be read, until you've imported
/// it and removed it.
#[test]
fn prune_drops_what_it_keeps_of_prs_long_closed() {
    let f = Fixture::new();
    f.prepare();
    f.add_drafts();
    let mut session = f.saved_session();
    session["state"] = json!("MERGED");
    f.save_session(&session);
    let prune = |args: &[&str]| {
        let mut all = vec!["prune"];
        all.extend(args);
        let run = f.run(&all);
        run.ok();
        run.lines
    };
    age_session(&f, 10);
    assert_eq!(prune(&[]), [json!({"pruned": []})]);
    let old = f.kept("comments.json");
    for (kept, why) in [
        (
            r#"{"version":1,"comments":[{"id":"d","path":"a.rs","text":"Draft","author":"","created":"t"}]}"#,
            "a draft",
        ),
        ("not JSON", "a file it can't read"),
    ] {
        fs::write(&old, kept).expect("write");
        assert_eq!(prune(&["--days", "5"]), [json!({"pruned": []})], "{why}");
    }
    // Only what GitHub has: it comes back with a pull.
    fs::write(
        &old,
        r#"{"version":1,"comments":[{"id":"a","path":"a.rs","text":"Done","author":"","created":"t","ref":"github:1"}]}"#,
    )
    .expect("write");
    age_session(&f, 40);
    assert!(has_pull_ref(&f));
    assert_eq!(prune(&[]), [json!({"pruned": [7]})]);
    assert!(!f.kept("").exists());
    assert!(!has_pull_ref(&f));
    assert_eq!(f.comments()["comments"].as_array().map(Vec::len), Some(8));
}

/// `list` leaves counting your drafts to dffy, which keeps them; after
/// answering it prunes, at most once a day.
#[test]
fn list_prunes_once_a_day() {
    let f = Fixture::new();
    f.prepare();
    f.add_drafts();
    f.reply("List", None, "list.json");
    let run = f.run(&["list"]);
    run.ok();
    let reviews = run.lines[0]["reviews"].as_array().expect("reviews").clone();
    assert!(
        reviews.iter().all(|r| r.get("drafts").is_none()),
        "{reviews:?}"
    );

    // Pruned today already, so not again.
    let mut session = f.saved_session();
    session["state"] = json!("CLOSED");
    f.save_session(&session);
    age_session(&f, 40);
    f.run(&["list"]).ok();
    assert!(f.kept("session.json").exists(), "kept until tomorrow");

    let stamp = f.kept("../pruned");
    fs::File::options()
        .write(true)
        .open(&stamp)
        .and_then(|s| s.set_modified(SystemTime::now() - Duration::from_secs(2 * 86_400)))
        .expect("a day later");
    f.run(&["list"]).ok();
    assert!(!f.kept("").exists());
}

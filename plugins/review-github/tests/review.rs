//! The built binary end to end, as dffy runs it: stdin closed,
//! `DFFY_REVIEW_PROTOCOL=1`, JSON lines on stdout. A real git repository
//! whose `origin` is a local bare repository holding the PR's ref, a stub
//! `gh` on PATH that replays fixtures in GitHub's GraphQL shape and logs
//! each request, and the dffy built with the workspace for `dffy comments`.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

const PR: &str = "7";

const URL: &str = "https://github.com/acme/widgets/pull/7";

/// Each `gh` event records how many lines the plugin had printed by then,
/// which orders the plugin's output against its requests without sleeps.
const GH: &str = r#"#!/bin/sh
body=$(cat)
op=$(printf '%s' "$body" | sed -n 's/.*"operationName":"\([A-Za-z]*\)".*/\1/p')
printf '%s\n' "$body" >> "$STUB/requests"
printf 'gh %s after %s\n' "$op" "$(wc -l < "$STUB/out" | tr -d ' ')" >> "$STUB/events"
n=$(grep -c "\"operationName\":\"$op\"" "$STUB/requests")
for f in "$STUB/gh/$op.$n" "$STUB/gh/$op"; do
  if [ -f "$f.json" ]; then cat "$f.json"; exit 0; fi
  if [ -f "$f.error.json" ]; then cat "$f.error.json"; echo "gh: GraphQL error" >&2; exit 1; fi
  if [ -f "$f.down" ]; then echo 'error connecting to api.github.com' >&2; exit 1; fi
done
echo "gh stub: no fixture for $op call $n" >&2
exit 1
"#;

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

fn numbered(lines: impl IntoIterator<Item = String>) -> String {
    lines.into_iter().map(|l| l + "\n").collect()
}

/// The plugin's stdout: one JSON object a line, each with the protocol's
/// `version`, which is taken out so tests compare the rest.
fn lines(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|l| {
            let mut line: Value = serde_json::from_str(l).expect("a line is JSON");
            let object = line.as_object_mut().expect("a line is an object");
            assert_eq!(object.remove("version"), Some(json!(1)), "{l}");
            line
        })
        .collect()
}

/// A run's exit status and lines.
struct Run {
    code: Option<i32>,
    lines: Vec<Value>,
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
        git(&author, &["add", "."]);
        git(&author, &["commit", "-q", "-m", "base"]);
        let base = git(&author, &["rev-parse", "HEAD"]);
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
        fs::create_dir_all(d.join("stub/bin")).expect("stub dir");
        fs::create_dir_all(d.join("stub/gh")).expect("stub dir");
        let gh = d.join("stub/bin/gh");
        fs::write(&gh, GH).expect("stub");
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("chmod");
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

    /// A verb as dffy runs it, with stdout written to a file the `gh` stub
    /// counts lines in.
    fn run(&self, args: &[&str]) -> Run {
        let out = self.path("stub/out");
        let file = fs::File::create(&out).expect("stdout file");
        let path = std::env::var("PATH").unwrap_or_default();
        let status = Command::new(env!("CARGO_BIN_EXE_dffy-review-github"))
            .current_dir(self.work())
            .args(args)
            .env(
                "PATH",
                format!("{}:{path}", self.path("stub/bin").display()),
            )
            .env("STUB", self.path("stub"))
            .env("DFFY_REVIEW_PROTOCOL", "1")
            .env("XDG_DATA_HOME", self.path("data"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .stdout(file)
            .status()
            .expect("runs");
        Run {
            code: status.code(),
            lines: lines(&fs::read(out).expect("stdout")),
        }
    }

    /// The comments file the plugin keeps for the PR: under the data
    /// directory, one directory per repository and PR.
    fn comments_file(&self) -> PathBuf {
        let sessions = self.path("data/dffy-review-github");
        let repo = fs::read_dir(&sessions)
            .expect("a session")
            .next()
            .expect("one repository")
            .expect("entry")
            .path();
        repo.join(PR).join("comments.json")
    }

    /// The session as the plugin keeps it, beside the comments file.
    fn saved_session(&self) -> Value {
        let path = self.comments_file().with_file_name("session.json");
        serde_json::from_slice(&fs::read(path).expect("a session")).expect("JSON")
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

    fn operations(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| r["operationName"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    /// A cold `prepare` of the PR with the two pages of threads.
    fn prepare(&self) -> Run {
        self.reply("PullRequest", Some(1), "pull_request_page_1.json");
        self.reply("PullRequest", Some(2), "pull_request_page_2.json");
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

#[test]
fn prepare_prints_the_session_once_the_head_is_local_then_imports_the_rest() {
    let f = Fixture::new();
    let run = f.prepare();

    assert_eq!(
        run.lines,
        [
            json!({"session": {"number": 7, "title": "Spell out five", "url": URL,
                "base": f.base, "head": f.head, "last_reviewed_head": null,
                "comments": f.comments_file()}}),
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
    assert_eq!(warm.lines[1..], [json!({"progress": "0/0"})]);
    let check = f.run(&["check", PR]);
    assert_eq!(check.lines, [json!({"head": f.head2, "moved": true})]);
}

#[test]
fn check_asks_git_whether_the_head_moved_and_never_the_api() {
    let f = Fixture::new();
    f.prepare();
    let before = f.requests().len();

    let run = f.run(&["check", PR]);
    run.ok();
    assert_eq!(run.lines, [json!({"head": f.head, "moved": false})]);

    f.move_head();
    let run = f.run(&["check", PR]);
    run.ok();
    assert_eq!(run.lines, [json!({"head": f.head2, "moved": true})]);
    assert_eq!(f.requests().len(), before);
    // Nothing was fetched.
    assert_eq!(
        git(&f.work(), &["rev-parse", &format!("refs/dffy/pull/{PR}")]),
        f.head
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
            json!({"session": {"number": 7, "title": "Spell out five", "url": URL,
                "base": f.base, "head": f.head2, "last_reviewed_head": null,
                "comments": f.comments_file()}}),
            json!({"progress": "0/0"}),
        ]
    );
    let check = f.run(&["check", PR]);
    assert_eq!(check.lines, [json!({"head": f.head2, "moved": false})]);
    assert_eq!(f.run(&["prepare", PR]).session(), run.session());
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
            json!({"posted": 3, "failed": 0, "url": format!("{URL}#pullrequestreview-2001")}),
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
    assert_eq!(f.draft(&ids[1])["ref"], "github:PRRC_kwDOAAAAB84AAAC3");
    assert_eq!(f.draft(&ids[1])["url"], format!("{URL}#discussion_r1103"));
    assert_eq!(f.draft(&ids[2])["ref"], "github:PRRC_kwDOAAAAB84AAAC2");
    assert_eq!(
        f.comment("github:PRRC_kwDOAAAAB84AAAB1")["github_resolved"],
        true
    );

    // A retry finds nothing left to send, and points at the PR.
    let before = f.operations().len();
    let run = f.post("pending", "");
    run.ok();
    assert_eq!(run.lines, [json!({"posted": 0, "failed": 0, "url": URL})]);
    assert_eq!(f.operations()[before..], ["PostState"]);
}

#[test]
fn a_rejected_comment_stays_a_draft_and_a_retry_posts_only_it_in_the_pending_review() {
    let f = Fixture::new();
    f.prepare();
    let ids = f.add_drafts();
    f.reply("PostState", Some(1), "post_state.json");
    f.reply("PostState", Some(2), "post_state_pending.json");
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
            json!({"posted": 2, "failed": 1, "pending_review": true,
                "url": format!("{URL}#pullrequestreview-2001")}),
        ]
    );
    let rejected = f.draft(&ids[0]);
    assert!(rejected.get("ref").is_none(), "{rejected}");
    assert_eq!(rejected["text"], "Needs a test.");
    assert_eq!(f.draft(&ids[2])["ref"], "github:PRRC_kwDOAAAAB84AAAC2");
    let notice = f.comments()["notice"]
        .as_str()
        .expect("a notice")
        .to_owned();
    assert!(notice.contains("Line could not be resolved"), "{notice}");
    assert!(
        !f.operations()
            .iter()
            .any(|o| o == "SubmitPullRequestReview"),
        "the review stays pending while a comment is missing from it"
    );

    let run = f.post("approve", "");
    run.ok();
    assert_eq!(
        *run.last(),
        json!({"posted": 1, "failed": 0, "url": format!("{URL}#pullrequestreview-2001")})
    );
    assert_eq!(f.draft(&ids[0])["ref"], "github:PRRC_kwDOAAAAB84AAAC1");
    let sent = f.operations();
    let count = |op: &str| sent.iter().filter(|o| *o == op).count();
    assert_eq!(count("AddReview"), 1, "the retry reuses the pending review");
    assert_eq!(count("AddPullRequestReviewThread"), 3);
    assert_eq!(count("AddPullRequestReviewThreadReply"), 1);
    assert_eq!(count("SubmitPullRequestReview"), 1);
    let last_thread = f
        .requests()
        .into_iter()
        .rfind(|r| r["operationName"] == "AddPullRequestReviewThread")
        .expect("a thread");
    assert_eq!(last_thread["variables"]["review"], "PRR_kwDOAAAAB84AAAAR");
    assert!(f.comments().get("notice").is_none());
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
        json!({"posted": 2, "failed": 1, "pending_review": true,
            "url": format!("{URL}#pullrequestreview-2001")})
    );
    assert!(run.error().contains("api.github.com"), "{}", run.error());
    assert!(f.draft(&ids[2]).get("ref").is_none());

    let run = f.post("comment", "");
    run.ok();
    assert_eq!(
        *run.last(),
        json!({"posted": 1, "failed": 0, "url": format!("{URL}#pullrequestreview-2001")})
    );
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
    assert_eq!(root["github_resolved"], false);
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
            json!({"posted": 1, "failed": 1, "url": format!("{URL}#pullrequestreview-2001")}),
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
    assert!(f.draft(&local).get("ref").is_none());
    let notice = f.comments()["notice"]
        .as_str()
        .expect("a notice")
        .to_owned();
    assert!(
        notice.contains("src/lib.rs:3: lines not in the PR head (unpushed changes?)"),
        "{notice}"
    );
    assert_eq!(
        f.saved_session()["last_reviewed_head"],
        f.head.as_str(),
        "one posted"
    );
}

/// `--dry-run` says what `post` would do with each draft, from git alone:
/// on its line at the PR's head, as a file comment, or not at all and why,
/// then the counts. Nothing is asked of GitHub and nothing is written.
#[test]
fn a_dry_run_plans_each_draft_from_git_alone() {
    let f = Fixture::new();
    f.prepare();
    let ids = f.add_planned_drafts();
    let before = (f.requests().len(), f.comments());

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
            json!({"planned": {"line": 2, "file": 2, "skip": 3}}),
        ]
    );
    assert_eq!((f.requests().len(), f.comments()), before);
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
        json!({"posted": 4, "failed": 3, "url": format!("{URL}#pullrequestreview-2001")})
    );
    assert_eq!(
        f.operations().last().map(String::as_str),
        Some("SubmitPullRequestReview"),
        "drafts planned out don't hold the event back"
    );
}

/// After a post that left the review pending, `--submit-only` submits it
/// with the event and summary, without posting the drafts still left.
#[test]
fn submit_only_submits_the_pending_review_without_posting_drafts() {
    let f = Fixture::new();
    f.prepare();
    let ids = f.add_drafts();
    f.reply("PostState", Some(1), "post_state.json");
    f.reply("PostState", Some(2), "post_state_pending.json");
    f.reply("AddReview", None, "add_review.json");
    f.reply(
        "AddPullRequestReviewThread",
        Some(1),
        "add_thread_rejected.json",
    );
    f.reply("AddPullRequestReviewThread", Some(2), "add_thread_2.json");
    f.reply("AddPullRequestReviewThreadReply", None, "add_reply.json");
    f.reply("SubmitPullRequestReview", None, "submit.json");
    assert_eq!(f.post("approve", "").last()["pending_review"], true);
    let before = f.operations().len();

    let file = f.summary("Ship it\n");
    let run = f.run(&[
        "post",
        PR,
        "--submit-only",
        "--event",
        "approve",
        "--summary-file",
        &file,
    ]);
    run.ok();
    assert_eq!(
        run.lines,
        [json!({"posted": 0, "failed": 0, "url": format!("{URL}#pullrequestreview-2001")})]
    );
    assert_eq!(
        f.operations()[before..],
        ["PostState", "SubmitPullRequestReview"]
    );
    assert_eq!(
        f.requests().last().expect("the submit")["variables"],
        json!({"review": "PRR_kwDOAAAAB84AAAAR", "event": "APPROVE", "body": "Ship it"})
    );
    assert!(f.draft(&ids[0]).get("ref").is_none(), "still a draft");
    assert!(
        f.comments().get("notice").is_none(),
        "the review isn't pending any more"
    );
}

/// `detect` answers for the branch checked out: from the session kept for
/// it without asking GitHub, else from one query; on another branch with
/// no PR, or a detached HEAD, with none.
#[test]
fn detect_finds_the_branchs_pr_from_its_session_else_by_one_query() {
    let f = Fixture::new();
    git(&f.work(), &["checkout", "-q", "-b", "five"]);
    f.reply("Detect", None, "detect.json");
    let run = f.run(&["detect"]);
    run.ok();
    let mut queried = run.lines[0]["pr"].clone();
    let comments = queried
        .as_object_mut()
        .and_then(|pr| pr.remove("comments"))
        .expect("the comments file");
    assert_eq!(
        queried,
        json!({"number": 7, "title": "Spell out five", "url": URL,
            "base": f.base, "head": f.head, "last_reviewed_head": null})
    );
    assert_eq!(f.operations(), ["Detect"]);

    let prepared = f.prepare();
    assert_eq!(comments, json!(f.comments_file()));
    let before = f.operations().len();
    let run = f.run(&["detect"]);
    run.ok();
    assert_eq!(run.lines, [json!({"pr": prepared.session()})]);
    assert_eq!(f.operations().len(), before, "from the session");

    git(&f.work(), &["checkout", "-q", "--detach"]);
    assert_eq!(f.run(&["detect"]).lines, [json!({"pr": null})]);
}

#[test]
fn list_shows_prs_waiting_on_you_yours_and_the_current_branchs_in_one_query() {
    let f = Fixture::new();
    f.reply("List", None, "list.json");
    let run = f.run(&["list"]);
    run.ok();
    let pr = |n: u64, title: &str, author: &str, head: &str, reason: &str| {
        json!({"number": n, "title": title, "author": author,
            "url": format!("https://github.com/acme/widgets/pull/{n}"),
            "head_ref": head, "reason": reason})
    };
    assert_eq!(
        run.lines,
        [json!({"prs": [
            pr(7, "Spell out five", "octocat", "five", "current_branch"),
            pr(9, "Add a cache", "hubot", "cache", "review_requested"),
            pr(11, "Document the cache", "ada", "docs", "mine"),
        ]})]
    );
    let vars = &f.requests()[0]["variables"];
    assert_eq!(
        *vars,
        json!({"owner": "acme", "name": "widgets", "branch": "main", "onBranch": true,
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

/// Unknown verbs and events, missing arguments, and a dffy speaking another
/// protocol get exit 2 and an error line, before anything is read or sent.
#[test]
fn what_the_plugin_doesnt_speak_exits_2_with_an_error_line() {
    for (args, protocol, says) in [
        (&["frobnicate"][..], "1", "unknown verb frobnicate"),
        (&["open", PR], "1", "unknown verb open"),
        (&["prepare"], "1", "usage"),
        (
            &["post", PR, "--event", "merge"],
            "1",
            "unknown event merge",
        ),
        (&["post", PR], "1", "usage"),
        (
            &["post", PR, "--submit-only", "--event", "pending"],
            "1",
            "--submit-only submits",
        ),
        (
            &["post", PR, "--dry-run", "--event", "comment"],
            "1",
            "usage",
        ),
        (
            &["list"],
            "2",
            "speaks review protocol 1, and dffy asked for 2",
        ),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_dffy-review-github"))
            .args(args)
            .env("DFFY_REVIEW_PROTOCOL", protocol)
            .env("PATH", "/nonexistent")
            .stdin(Stdio::null())
            .output()
            .expect("runs");
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let lines = lines(&out.stdout);
        assert_eq!(lines.len(), 1, "{args:?}: {lines:?}");
        let error = lines[0]["error"].as_str().expect("an error line");
        assert!(error.contains(says), "{args:?}: {error}");
    }
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

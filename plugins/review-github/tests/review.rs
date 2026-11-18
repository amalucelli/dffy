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
/// GraphQL requests go to `requests` and `events`, REST ones to `rest`.
const GH: &str = r#"#!/bin/sh
case "$1" in
  --version) echo 'gh version 2.0.0 (stub)'; exit 0 ;;
  auth)
    printf '%s\n' "$*" >> "$STUB/auth"
    if [ -f "$STUB/gh/logged-out" ]; then
      echo 'You are not logged into any GitHub hosts. To log in, run: gh auth login' >&2; exit 1
    fi
    printf 'github.com\n  ✓ Logged in to github.com account octocat (keyring)\n'; exit 0 ;;
esac
body=$(cat)
if [ -f "$STUB/gh/logged-out" ]; then
  echo 'To get started with GitHub CLI, please run:  gh auth login' >&2; exit 4
fi
if [ -f "$STUB/gh/bad-token" ]; then echo 'gh: Bad credentials (HTTP 401)' >&2; exit 1; fi
if [ "$2" != graphql ]; then
  # REST, named by the path's last segment. A `.hold` makes the answer wait
  # for the plugin's first line; the log says how many lines it had then.
  op=$(printf '%s' "$2" | sed 's/?.*//; s|.*/||')
  n=$(($(cat "$STUB/rest" 2>/dev/null | grep -c "/$op?") + 1))
  if [ -f "$STUB/gh/$op.hold" ]; then
    i=0
    while [ "$(wc -l < "$STUB/out")" -lt 1 ] && [ $i -lt 30 ]; do sleep 0.1; i=$((i + 1)); done
  fi
  printf '%s after %s\n' "$*" "$(wc -l < "$STUB/out" | tr -d ' ')" >> "$STUB/rest"
  for f in "$STUB/gh/$op.$n" "$STUB/gh/$op"; do
    if [ -f "$f.json" ]; then cat "$f.json"; exit 0; fi
    if [ -f "$f.down" ]; then echo 'error connecting to api.github.com' >&2; exit 1; fi
  done
  echo "gh stub: no fixture for $op call $n" >&2
  exit 1
fi
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

    /// Makes `gh` answer as it does without a token.
    fn log_out(&self) {
        fs::write(self.path("stub/gh/logged-out"), "").expect("write");
    }

    /// A verb as dffy runs it, with stdout written to a file the `gh` stub
    /// counts lines in.
    fn run(&self, args: &[&str]) -> Run {
        let path = std::env::var("PATH").unwrap_or_default();
        self.run_on(args, &format!("{}:{path}", self.path("stub/bin").display()))
    }

    /// A verb run with nothing on PATH but git and dffy: a `gh` installed
    /// beside git, as on CI, would otherwise be found.
    fn run_without_gh(&self, args: &[&str]) -> Run {
        let bin = self.path("nogh");
        if !bin.exists() {
            fs::create_dir(&bin).expect("dir");
            let git = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|dir| dir.join("git"))
                .find(|git| git.is_file())
                .expect("git on PATH");
            std::os::unix::fs::symlink(git, bin.join("git")).expect("git");
            std::os::unix::fs::symlink(&self.real_dffy, bin.join("dffy")).expect("dffy");
        }
        self.run_on(args, &bin.display().to_string())
    }

    fn run_on(&self, args: &[&str], path: &str) -> Run {
        let out = self.path("stub/out");
        let file = fs::File::create(&out).expect("stdout file");
        let status = Command::new(env!("CARGO_BIN_EXE_dffy-review-github"))
            .current_dir(self.work())
            .args(args)
            .env("PATH", path)
            .env_remove("GH_HOST")
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
        let real = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join("git"))
            .find(|git| git.is_file())
            .expect("git on PATH");
        let wrapper = self.path("stub/bin/git");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\necho \"$* after $(wc -l < \"$STUB/out\" | tr -d ' ')\" >> \"$STUB/git\"\n\
                 exec '{}' \"$@\"\n",
                real.display()
            ),
        )
        .expect("wrapper");
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

/// `describe` names the command dffy adds, the words its screens use, a
/// PR as `PR #7`, and the events the post dialog offers: `pending` leaves
/// the review open, the others submit it. dffy runs it from anywhere, so
/// it needs no repository, git or gh.
#[test]
fn describe_names_the_command_from_nothing_but_itself() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = Command::new(env!("CARGO_BIN_EXE_dffy-review-github"))
        .current_dir(dir.path())
        .arg("describe")
        .env("PATH", "")
        .env("DFFY_REVIEW_PROTOCOL", "1")
        .stdin(Stdio::null())
        .output()
        .expect("runs");
    assert!(out.status.success(), "{out:?}");
    let [described] = &lines(&out.stdout)[..] else {
        panic!("one line: {out:?}");
    };
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
            json!({"session": {"id": "7", "title": "Spell out five", "url": URL,
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
            json!({"posted": 2, "failed": 1, "held": true, "held_message": "review still pending",
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
        json!({"posted": 2, "failed": 1, "held": true, "held_message": "review still pending",
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

/// `--dry-run` says what `post` would do with each draft, from git and
/// what `prepare` kept: on its line at the PR's head, as a file comment, or
/// not at all and why, then the counts. Nothing is asked of GitHub and
/// nothing is written.
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
            json!({"planned": {"line": 2, "file": 2, "skip": 3}}),
        ]
    );
    assert_eq!((f.requests().len(), f.rest().len(), f.comments()), before);
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
            format!("fetch --quiet --no-tags origin {} {}", both[0], both[1]),
            format!("fetch --quiet --no-tags origin {rewritten}"),
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
    assert_eq!(calls, [format!("fetch --quiet --no-tags origin {GONE}")]);
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
            "remote get-url origin".to_owned(),
            format!("cat-file -e {base}^{{commit}}"),
            format!("cat-file -e {head}^{{commit}}"),
            format!("cat-file -e {head}^{{commit}}"),
            "cat-file --batch".to_owned(),
            format!("merge-base {base} {head}"),
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
            "api repos/acme/widgets/pulls/{PR}/files?per_page=100 --paginate after "
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
            json!({"planned": {"line": 3, "file": 1, "skip": 0}}),
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

    f.down("files", Some(3));
    f.run(&["pull", PR]).ok();
    let planned = plan();
    assert_eq!(planned["plan"], "line");
    assert_eq!(planned["unsure"], true);
}

/// The PR's files are asked for beside its threads and its fetch, so the
/// session line doesn't wait for them.
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

/// After a post that held the event back, leaving the review pending,
/// `--apply-event` submits it with the event and summary, without posting
/// the drafts still left.
#[test]
fn apply_event_submits_the_pending_review_without_posting_drafts() {
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
    assert_eq!(f.post("approve", "").last()["held"], true);
    let before = f.operations().len();

    let file = f.summary("Ship it\n");
    let run = f.run(&[
        "post",
        PR,
        "--apply-event",
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
    let mut queried = run.lines[0]["review"].clone();
    let comments = queried
        .as_object_mut()
        .and_then(|pr| pr.remove("comments"))
        .expect("the comments file");
    assert_eq!(
        queried,
        json!({"id": "7", "title": "Spell out five", "url": URL,
            "base": f.base, "head": f.head, "last_reviewed_head": null})
    );
    assert_eq!(f.operations(), ["Detect"]);

    let prepared = f.prepare();
    assert_eq!(comments, json!(f.comments_file()));
    let before = f.operations().len();
    let run = f.run(&["detect"]);
    run.ok();
    assert_eq!(run.lines, [json!({"review": prepared.session()})]);
    assert_eq!(f.operations().len(), before, "from the session");

    git(&f.work(), &["checkout", "-q", "--detach"]);
    assert_eq!(f.run(&["detect"]).lines, [json!({"review": null})]);
}

#[test]
fn list_shows_prs_waiting_on_you_yours_and_the_current_branchs_in_one_query() {
    let f = Fixture::new();
    f.reply("List", None, "list.json");
    let run = f.run(&["list"]);
    run.ok();
    let pr = |n: u64, title: &str, author: &str, head: &str, reason: &str| {
        json!({"id": n.to_string(), "title": title, "author": author,
            "url": format!("https://github.com/acme/widgets/pull/{n}"),
            "head_ref": head, "reason": reason})
    };
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
            &["post", PR, "--apply-event", "--event", "pending"],
            "1",
            "--apply-event submits",
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
            json!({"planned": {"line": 1, "file": 0, "skip": 0}}),
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
    assert_eq!(
        run.last().get("held"),
        None,
        "pending is left pending as asked"
    );
    assert!(f.draft(yours.as_str().expect("an id")).get("ref").is_some());
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

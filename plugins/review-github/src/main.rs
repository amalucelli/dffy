//! `dffy-review-github`, an experimental review plugin for dffy: it keeps
//! dffy's review comments file for a GitHub pull request in step with the
//! PR's review threads, and posts your drafts. dffy runs it in the
//! background (review plugin protocol 1) and reads JSON lines from its
//! stdout; it never prompts or draws. GitHub is reached only through `gh`,
//! the code only through git, and the comments file is read and written
//! only through `dffy comments`, which owns its format and lock.

mod git;
mod github;

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::git::{Remote, Source};
use crate::github::{Error, Page};

const USAGE: &str = "usage: dffy-review-github describe | doctor | list | detect | prepare <pr> \
                     | check <pr> | pull <pr> \
                     | post <pr> --event pending|comment|approve|request_changes [--summary-file <file>] \
                     | post <pr> --dry-run \
                     | post <pr> --apply-event --event comment|approve|request_changes [--summary-file <file>] \
                     | prefetch";

/// What `describe` says: the command, the words for a PR, and the events
/// the post dialog offers, `pending` leaving the review open on GitHub.
fn description() -> Value {
    let event = |id: &str, key: &str, label: &str, done: &str| json!({"id": id, "key": key, "label": label, "done": done});
    let mut pending = event(
        "pending",
        "p",
        "pending: on GitHub, for you only",
        "review pending",
    );
    pending["submits"] = json!(false);
    json!({
        "command": "pr",
        "noun": "pull request",
        "nouns": "pull requests",
        "short": "PR",
        "id_prefix": "#",
        "events": [
            pending,
            event("comment", "c", "comment", "commented"),
            event("approve", "a", "approve", "approved"),
            event("request_changes", "r", "request changes", "changes requested"),
        ],
        "outdated_reply_warning": "GitHub collapses this thread, so a reply there is easy to miss",
    })
}

/// What dffy shows when refused drafts held the event back.
const HELD: &str = "review still pending";

const VERBS: [&str; 9] = [
    "describe", "doctor", "list", "detect", "prepare", "check", "pull", "post", "prefetch",
];

/// The review plugin protocol this speaks. dffy names the one it wants in
/// `DFFY_REVIEW_PROTOCOL`, and every line printed carries this one.
const PROTOCOL: u64 = 1;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if args == ["--version"] {
        println!("dffy-review-github {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    // Unset when you run a verb yourself.
    if let Some(asked) = std::env::var_os("DFFY_REVIEW_PROTOCOL")
        .filter(|v| v.to_str().and_then(|v| v.parse().ok()) != Some(PROTOCOL))
    {
        return fail(
            2,
            &format!(
                "dffy-review-github speaks review protocol {PROTOCOL}, and dffy asked for {}; \
                 install the dffy-review-github that matches your dffy",
                asked.to_string_lossy()
            ),
        );
    }
    let pr = args
        .get(1)
        .and_then(|n| n.trim_start_matches('#').parse().ok());
    let result = match (args.as_slice(), pr) {
        (["describe"], _) => {
            emit(description());
            Ok(())
        }
        (["doctor"], _) => {
            doctor();
            Ok(())
        }
        (["list"], _) => list(),
        (["detect"], _) => detect(),
        (["prefetch"], _) => prefetch(),
        (["prepare", _], Some(pr)) => prepare(pr),
        (["check", _], Some(pr)) => check(pr),
        (["pull", _], Some(pr)) => Review::new(pr).and_then(|review| pull(&review)),
        (["post", _, flags @ ..], Some(pr)) => match post_args(flags) {
            Ok((PostMode::Post(event), summary)) => post(pr, event, summary),
            Ok((PostMode::DryRun, _)) => dry_run(pr),
            Ok((PostMode::ApplyEvent(event), summary)) => apply_event(pr, event, summary),
            Err(e) => return fail(2, &e),
        },
        ([verb, ..], _) if !VERBS.contains(verb) => {
            return fail(2, &format!("unknown verb {verb}; {USAGE}"));
        }
        _ => return fail(2, USAGE),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(1, &e),
    }
}

/// Prints one line of the protocol.
fn emit(mut line: Value) {
    line["version"] = json!(PROTOCOL);
    // dffy may stop reading after the first line, and the work goes on.
    writeln!(std::io::stdout(), "{line}").ok();
}

fn fail(code: u8, message: &str) -> ExitCode {
    emit(json!({"error": message}));
    ExitCode::from(code)
}

fn dffy_comments(file: &Path) -> Command {
    let mut cmd = Command::new("dffy");
    cmd.arg("--comments").arg(file).arg("comments");
    cmd
}

/// The comments file as `dffy comments --json` prints it.
fn read(file: &Path) -> Result<Value, String> {
    let out = dffy_comments(file)
        .arg("--json")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("can't run dffy: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "dffy comments: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("dffy comments --json: {e}"))
}

fn comments(file: &Value) -> &[Value] {
    file["comments"].as_array().map_or(&[], Vec::as_slice)
}

/// Writes `ops` with `dffy comments apply`, all or none.
fn apply(file: &Path, ops: &[Value]) -> Result<(), String> {
    let mut child = dffy_comments(file)
        .arg("apply")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("can't run dffy: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(Value::from(ops).to_string().as_bytes())
            .map_err(|e| format!("can't write to dffy: {e}"))?;
    }
    let out = child.wait_with_output().map_err(|e| format!("dffy: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "dffy comments apply: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn notice(file: &Path, notice: Option<&str>) -> Result<(), String> {
    apply(file, &[json!({"op": "notice", "notice": notice})])
}

/// A thread resolved or reopened in dffy and not sent yet: its `resolved`
/// differs from GitHub's as last seen.
fn toggled(comment: &Value) -> bool {
    comment["github_thread"].is_string()
        && comment["resolved"].as_bool().unwrap_or_default()
            != comment["github_resolved"].as_bool().unwrap_or_default()
}

/// A PR under review, and what's kept for it: the comments file, drafts
/// included, the session dffy was last given, and GitHub's hunks of a
/// head for `post` to plan with. It's in the data
/// directory, not the state or cache one, because the drafts are your
/// writing.
struct Review {
    remote: Remote,
    pr: u64,
    dir: PathBuf,
}

/// Where the PRs of `remote`'s repository are kept, one directory each.
fn repository_dir(remote: &Remote) -> Result<PathBuf, String> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok_or("neither XDG_DATA_HOME nor HOME is set")?;
    Ok(data.join("dffy-review-github").join(&remote.key))
}

impl Review {
    fn new(pr: u64) -> Result<Self, String> {
        Self::of(Remote::origin()?, pr)
    }

    fn of(remote: Remote, pr: u64) -> Result<Self, String> {
        Ok(Self {
            dir: repository_dir(&remote)?.join(pr.to_string()),
            remote,
            pr,
        })
    }

    fn file(&self) -> PathBuf {
        self.dir.join("comments.json")
    }

    /// The session line's `session`, without `comments`, as last printed,
    /// with the PR's branch in `head_ref`, which `detect` looks for.
    fn session(&self) -> Option<Value> {
        serde_json::from_slice(&fs::read(self.dir.join("session.json")).ok()?).ok()
    }

    /// `session` as dffy reads it: the PR's number as its id, with the
    /// comments file, without the branch.
    fn printed(&self, session: &Value) -> Value {
        let mut session = session.clone();
        if let Some(fields) = session.as_object_mut() {
            fields.remove("head_ref");
            fields.remove("number");
        }
        session["id"] = json!(self.pr.to_string());
        session["comments"] = json!(self.file().to_string_lossy());
        session
    }

    fn emit_session(&self, session: &Value) {
        emit(json!({ "session": self.printed(session) }));
    }

    fn save(&self, session: &Value) -> Result<(), String> {
        self.write("session.json", session)
    }

    fn write(&self, name: &str, value: &Value) -> Result<(), String> {
        let path = self.dir.join(name);
        let tmp = self.dir.join(format!("{name}.tmp"));
        fs::create_dir_all(&self.dir)
            .and_then(|()| fs::write(&tmp, value.to_string()))
            .and_then(|()| fs::rename(&tmp, &path))
            .map_err(|e| format!("can't write {}: {e}", path.display()))
    }

    /// Keeps GitHub's hunks of the PR at `head`, for `post` to plan with,
    /// in place of any kept before.
    fn keep_hunks(
        &self,
        head: &str,
        files: &HashMap<String, Vec<git::Hunk>>,
    ) -> Result<(), String> {
        let files: serde_json::Map<String, Value> = files
            .iter()
            .map(|(path, hunks)| {
                let hunks: Vec<Value> = hunks
                    .iter()
                    .map(|h| json!([h.old.start, h.old.end, h.new.start, h.new.end]))
                    .collect();
                (path.clone(), json!(hunks))
            })
            .collect();
        self.write("hunks.json", &json!({"head": head, "files": files}))
    }

    /// GitHub's hunks by path, when they were kept for `head`.
    fn hunks(&self, head: &str) -> Option<HashMap<String, Vec<git::Hunk>>> {
        let kept: Value =
            serde_json::from_slice(&fs::read(self.dir.join("hunks.json")).ok()?).ok()?;
        if kept["head"] != head {
            return None;
        }
        let hunk = |h: &Value| {
            let n = |i: usize| h[i].as_u64();
            Some(git::Hunk {
                old: n(0)?..n(1)?,
                new: n(2)?..n(3)?,
            })
        };
        kept["files"]
            .as_object()?
            .iter()
            .map(|(path, hunks)| {
                let hunks = hunks.as_array()?.iter().map(hunk).collect::<Option<_>>()?;
                Some((path.clone(), hunks))
            })
            .collect()
    }

    /// Records the session's head, the one drafts are posted against, as
    /// the head last reviewed.
    fn reviewed(&self) -> Result<(), String> {
        let Some(mut session) = self.session() else {
            return Ok(());
        };
        session["last_reviewed_head"] = session["head"].clone();
        self.save(&session)
    }

    /// Saves and prints the session on `head`, keeping the head `post`
    /// last reviewed.
    fn start(
        &self,
        saved: Option<&Value>,
        (base, head): (&str, &str),
        title: &Value,
        url: &Value,
        head_ref: &Value,
    ) -> Result<(), String> {
        let last_reviewed = saved.map_or(Value::Null, |s| s["last_reviewed_head"].clone());
        let session = json!({"number": self.pr, "title": title, "url": url,
            "base": base, "head": head, "last_reviewed_head": last_reviewed,
            "head_ref": head_ref});
        self.save(&session)?;
        self.emit_session(&session);
        Ok(())
    }
}

/// Writes `page`, and the pages after it as they arrive, into the comments
/// file, with a progress line for each. Then fetches the original commits
/// git didn't have, which a force-push left off the PR's head, and places
/// the threads written on them. The fetched commits get no ref: once
/// placed, a thread's lines are in the file, and a re-import that can't
/// read them leaves them.
fn import(review: &Review, mut page: Page) -> Result<(), String> {
    let file = review.file();
    let mut known = github::Known::default();
    for c in comments(&read(&file)?) {
        let Some(reference) = c["ref"].as_str() else {
            continue;
        };
        for (set, has) in [
            (&mut known.toggled, toggled(c)),
            (&mut known.anchored, c["anchor"].is_object()),
            (&mut known.unplaced, c["github_unplaced"].is_string()),
        ] {
            if has {
                set.insert(reference.to_owned());
            }
        }
    }
    let mut source = Source::default();
    let (mut written, mut waiting) = (0, Vec::new());
    loop {
        let (ops, missing) = github::upserts(&page.threads, &page.base, &mut source, &mut known);
        if !ops.is_empty() {
            apply(&file, &ops)?;
        }
        waiting.extend(missing);
        written += page.threads.len();
        emit(json!({"progress": format!("{written}/{}", page.total)}));
        let Some(after) = page.next.take() else {
            break;
        };
        page = github::page(&review.remote, review.pr, Some(&after))?;
    }
    let mut commits: Vec<&str> = waiting.iter().filter_map(github::original_commit).collect();
    commits.sort_unstable();
    commits.dedup();
    // Threads on a commit that didn't arrive stay as they were written.
    if commits.is_empty() || !git::fetch_commits(&commits) {
        return Ok(());
    }
    // A fresh source: the first one keeps its misses.
    let (ops, _) = github::upserts(&waiting, &page.base, &mut Source::default(), &mut known);
    apply(&file, &ops)
}

/// GitHub's hunks of the PR, asked for in the background.
fn fetch_files(review: &Review) -> JoinHandle<Option<HashMap<String, Vec<git::Hunk>>>> {
    let (remote, pr) = (review.remote.clone(), review.pr);
    std::thread::spawn(move || github::files(&remote, pr))
}

/// Writes the threads from `page` on, then keeps the hunks `files` gets
/// when they're of the session's head. GitHub doesn't say which head its
/// files are of; they're taken as `page`'s, asked for at the same time.
fn import_with_files(
    review: &Review,
    page: Page,
    files: JoinHandle<Option<HashMap<String, Vec<git::Hunk>>>>,
    session_head: &str,
) -> Result<(), String> {
    let head = page.head.clone();
    let imported = import(review, page);
    if let (true, Ok(Some(hunks))) = (head == session_head, files.join()) {
        review.keep_hunks(&head, &hunks)?;
    }
    imported
}

/// A PR prepared before, with its commits still here, answers from the
/// session at once; the head stays the one dffy shows until `pull`.
fn prepare(pr: u64) -> Result<(), String> {
    let review = Review::new(pr)?;
    // A session without a url was saved with GitHub down, on a guessed base.
    let Some(session) = review.session().filter(|s| {
        s["url"].is_string()
            && [&s["base"], &s["head"]]
                .iter()
                .all(|oid| oid.as_str().is_some_and(git::has_commit))
    }) else {
        return pull(&review);
    };
    review.emit_session(&session);
    let files = fetch_files(&review);
    let page = github::page(&review.remote, pr, None)?;
    // Threads on a newer head read their anchors from it.
    if !git::has_commit(&page.head) {
        git::fetch_pulls(&[pr])?;
    }
    let head = session["head"].as_str().unwrap_or_default();
    import_with_files(&review, page, files, head)
}

/// Fetches the head, the first page of threads and the PR's files
/// together, prints the session once both commits are here, then writes
/// every page.
fn pull(review: &Review) -> Result<(), String> {
    let pr = review.pr;
    let saved = review.session();
    let files = fetch_files(review);
    let (fetched, first) = std::thread::scope(|s| {
        let fetch = s.spawn(|| git::fetch_pulls(&[pr]));
        let first = github::page(&review.remote, pr, None);
        (
            fetch
                .join()
                .unwrap_or_else(|_| Err("the fetch stopped".to_owned())),
            first,
        )
    });
    let fetched = fetched.and_then(|()| git::git(&["rev-parse", &git::pull_ref(pr)]));
    let page = match first {
        Ok(page) => page,
        Err(Error::Setup(e)) => return Err(e),
        Err(e) => {
            // The code still opens, on the base it had or the default branch's.
            let head = fetched?;
            let base = match saved.as_ref().and_then(|s| s["base"].as_str()) {
                Some(base) => base.to_owned(),
                None => git::git(&["rev-parse", "refs/remotes/origin/HEAD"])?,
            };
            let kept = |field: &str| saved.as_ref().map_or(Value::Null, |s| s[field].clone());
            review.start(
                saved.as_ref(),
                (&base, &head),
                &kept("title"),
                &kept("url"),
                &kept("head_ref"),
            )?;
            return Err(format!("couldn't reach GitHub, comments may be stale: {e}"));
        }
    };
    if !git::has_commit(&page.head) {
        fetched?;
        return Err(format!(
            "origin's refs/pull/{pr}/head isn't the PR's head {}; try again",
            page.head
        ));
    }
    if !git::has_commit(&page.base) {
        git::git(&["fetch", "--quiet", "--no-tags", "origin", &page.base])?;
    }
    review.start(
        saved.as_ref(),
        (&page.base, &page.head),
        &page.title,
        &page.url,
        &page.head_ref,
    )?;
    let head = page.head.clone();
    import_with_files(review, page, files, &head)
}

/// Whether origin's head for the PR is still the session's, from git alone.
fn check(pr: u64) -> Result<(), String> {
    let review = Review::new(pr)?;
    let session = review
        .session()
        .ok_or_else(|| format!("#{pr} isn't prepared; run prepare {pr} first"))?;
    let name = format!("refs/pull/{pr}/head");
    let head = git::git(&["ls-remote", "origin", &name])?
        .lines()
        .find_map(|l| l.split_once('\t').filter(|(_, r)| *r == name))
        .map(|(oid, _)| oid.to_owned())
        .ok_or_else(|| format!("origin has no {name}"))?;
    emit(json!({"moved": session["head"] != head.as_str(), "head": head}));
    Ok(())
}

/// The open PR of the branch checked out, for plain `dffy`: from a session
/// kept for that branch when there's one, which needs nothing from GitHub,
/// else from one query by the branch's name. `{"review": null}` without
/// one, or on a detached HEAD.
fn detect() -> Result<(), String> {
    let Ok(branch) = git::git(&["symbolic-ref", "--quiet", "--short", "HEAD"]) else {
        emit(json!({ "review": null }));
        return Ok(());
    };
    let remote = Remote::origin()?;
    let mut kept: Vec<u64> = fs::read_dir(repository_dir(&remote)?)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect();
    // A branch's newest PR first: older ones on it are likely closed.
    kept.sort_unstable_by(|a, b| b.cmp(a));
    for pr in kept {
        let review = Review::of(remote.clone(), pr)?;
        if let Some(session) = review
            .session()
            .filter(|s| s["head_ref"] == branch.as_str())
        {
            emit(json!({ "review": review.printed(&session) }));
            return Ok(());
        }
    }
    let Some(mut found) = github::detect(&remote, &branch)? else {
        emit(json!({ "review": null }));
        return Ok(());
    };
    let pr = found["number"].as_u64().unwrap_or_default();
    let review = Review::of(remote, pr)?;
    found["last_reviewed_head"] = review
        .session()
        .map_or(Value::Null, |s| s["last_reviewed_head"].clone());
    emit(json!({ "review": review.printed(&found) }));
    Ok(())
}

fn open_prs() -> Result<Vec<Value>, String> {
    let remote = Remote::origin()?;
    let branch = git::git(&["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
    Ok(github::list(&remote, branch.as_deref())?)
}

fn list() -> Result<(), String> {
    emit(json!({"reviews": open_prs()?}));
    Ok(())
}

/// Fetches the heads of the PRs `list` shows, so preparing one later only
/// has to ask GitHub for its threads.
fn prefetch() -> Result<(), String> {
    let prs: Vec<u64> = open_prs()?
        .iter()
        .filter_map(|pr| pr["id"].as_str()?.parse().ok())
        .collect();
    if !prs.is_empty() {
        git::fetch_pulls(&prs)?;
    }
    emit(json!({ "fetched": prs }));
    Ok(())
}

/// For `dffy doctor`: whether gh is installed, and logged in to the host
/// the API calls go to. `gh auth status` asks that host, so it isn't free;
/// no other verb runs it, they read the same from their first call.
fn doctor() {
    let check =
        |name: &str, ok: bool, detail: &str| json!({"name": name, "ok": ok, "detail": detail});
    let first_line = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(|l| l.trim().trim_start_matches('✓').trim().to_owned())
            .find(|l| !l.is_empty())
            .unwrap_or_default()
    };
    let gh = |args: &[&str]| {
        Command::new("gh")
            .args(args)
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(Stdio::null())
            .output()
    };
    let checks = match gh(&["--version"]) {
        Err(e) => {
            let detail = if e.kind() == std::io::ErrorKind::NotFound {
                github::NO_GH.to_owned()
            } else {
                format!("can't run gh: {e}")
            };
            [
                check("gh", false, &detail),
                check("auth", false, "needs gh"),
            ]
        }
        Ok(version) => {
            let host = github::host();
            let auth = match gh(&["auth", "status", "--hostname", &host]) {
                Ok(out) if out.status.success() => {
                    let said = String::from_utf8_lossy(&out.stdout);
                    let line = said
                        .lines()
                        .find(|l| l.contains("Logged in to"))
                        .map_or_else(|| first_line(&out.stdout), |l| first_line(l.as_bytes()));
                    check("auth", true, &line)
                }
                Ok(_) => check("auth", false, &github::logged_out()),
                Err(e) => check("auth", false, &format!("can't run gh: {e}")),
            };
            [
                check("gh", version.status.success(), &first_line(&version.stdout)),
                auth,
            ]
        }
    };
    emit(json!({ "checks": checks }));
}

/// GitHub allows 80 content-creating requests a minute; this stays under
/// it with room for your other clients.
#[derive(Default)]
struct Pace(VecDeque<Instant>);

impl Pace {
    const PER_MINUTE: usize = 60;

    fn wait(&mut self) {
        if self.0.len() == Self::PER_MINUTE
            && let Some(first) = self.0.pop_front()
        {
            std::thread::sleep(
                (first + Duration::from_secs(60)).saturating_duration_since(Instant::now()),
            );
        }
        self.0.push_back(Instant::now());
    }
}

#[derive(Clone, Copy)]
enum Event {
    Pending,
    /// GitHub's review event.
    Submit(&'static str),
}

/// What a `post` run does.
enum PostMode {
    Post(Event),
    /// Says what `post` would do with each draft, without asking GitHub.
    DryRun,
    /// Submits your pending review, posting no drafts.
    ApplyEvent(Event),
}

/// `--dry-run` alone, or `--event`, `--summary-file` and `--apply-event`
/// in any order.
fn post_args<'a>(flags: &[&'a str]) -> Result<(PostMode, Option<&'a str>), String> {
    if flags == ["--dry-run"] {
        return Ok((PostMode::DryRun, None));
    }
    let (mut event, mut summary, mut apply_only) = (None, None, false);
    let mut flags = flags.iter();
    while let Some(flag) = flags.next() {
        if *flag == "--apply-event" {
            apply_only = true;
            continue;
        }
        match (*flag, flags.next()) {
            ("--event", Some(name)) => {
                event = Some(match *name {
                    "pending" => Event::Pending,
                    "comment" => Event::Submit("COMMENT"),
                    "approve" => Event::Submit("APPROVE"),
                    "request_changes" => Event::Submit("REQUEST_CHANGES"),
                    _ => {
                        return Err(format!(
                            "unknown event {name}; pending, comment, approve or request_changes"
                        ));
                    }
                });
            }
            ("--summary-file", Some(file)) => summary = Some(*file),
            _ => return Err(USAGE.to_owned()),
        }
    }
    let mode = match (event, apply_only) {
        (None, _) => return Err(USAGE.to_owned()),
        (Some(Event::Pending), true) => {
            return Err(
                "--apply-event submits the review: comment, approve or request_changes".to_owned(),
            );
        }
        (Some(event), true) => PostMode::ApplyEvent(event),
        (Some(event), false) => PostMode::Post(event),
    };
    Ok((mode, summary))
}

fn read_summary(file: Option<&str>) -> Result<String, String> {
    Ok(file
        .map(|f| fs::read_to_string(f).map_err(|e| format!("can't read {f}: {e}")))
        .transpose()?
        .unwrap_or_default())
}

fn location(comment: &Value) -> String {
    let path = comment["path"].as_str().unwrap_or_default();
    if comment["in_reply_to"].is_string() {
        return format!("reply on {path}");
    }
    let side = comment["side"].as_str().unwrap_or("new");
    match comment["end"][side].as_u64() {
        Some(line) => format!("{path}:{line}"),
        None => path.to_owned(),
    }
}

/// `draft` on the lines it has at the PR's `head` (the merge base's with
/// `base`, for the old side), found by its anchor: drafts are written on
/// the lines dffy showed, which may be the working tree's, and GitHub
/// takes line numbers at the head. Where the recorded first line still has
/// the anchor's lines it stays; else the one place the anchor and its
/// context are, else the one place its lines are. `Err` when they aren't
/// at the head. Replies and file comments pass as they are.
fn at_head(
    draft: &Value,
    (base, head): (&str, &str),
    source: &mut Source,
) -> Result<Value, String> {
    let wanted: Vec<&str> = draft["anchor"]["lines"]
        .as_array()
        .map(|l| l.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let side = draft["side"].as_str().unwrap_or("new");
    let (Some(end), false, false) = (
        draft["end"][side].as_u64(),
        wanted.is_empty(),
        draft["in_reply_to"].is_string(),
    ) else {
        return Ok(draft.clone());
    };
    let missing = || "lines not in the PR head (unpushed changes?)".to_owned();
    let path = match draft["old_path"].as_str() {
        Some(old) if side == "old" => old,
        _ => draft["path"].as_str().unwrap_or_default(),
    };
    let commit = if side == "old" {
        source.merge_base(base, head).ok_or_else(missing)?
    } else {
        head.to_owned()
    };
    let lines = source.lines(&commit, path).ok_or_else(missing)?;
    let around = |key: &str| -> Vec<&str> {
        draft["anchor"][key]
            .as_array()
            .map(|l| l.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    };
    let (before, after) = (around("before"), around("after"));
    let n = wanted.len();
    let at = |i: usize| {
        lines[i..i + n]
            .iter()
            .map(String::as_str)
            .eq(wanted.iter().copied())
    };
    let found: Vec<usize> = (0..=lines.len().saturating_sub(n))
        .filter(|&i| i + n <= lines.len() && at(i))
        .collect();
    let in_context = |&i: &usize| {
        i >= before.len()
            && lines[i - before.len()..i]
                .iter()
                .map(String::as_str)
                .eq(before.iter().copied())
            && lines.len() >= i + n + after.len()
            && lines[i + n..i + n + after.len()]
                .iter()
                .map(String::as_str)
                .eq(after.iter().copied())
    };
    // 0-based, as `found` holds them.
    let recorded = usize::try_from(end).unwrap_or(0).checked_sub(n);
    let in_place: Vec<usize> = found.iter().copied().filter(in_context).collect();
    let ((Some(first), ..) | (None, &[first], _) | (None, _, &[first])) = (
        recorded.filter(|r| found.contains(r)),
        &in_place[..],
        &found[..],
    ) else {
        return Err(missing());
    };
    let mut placed = draft.clone();
    let delta = i64::try_from(first).unwrap_or(0)
        - recorded.and_then(|r| i64::try_from(r).ok()).unwrap_or(0);
    for key in ["start", "end"] {
        if let Some(line) = placed[key][side].as_i64() {
            placed[key][side] = json!(line + delta);
        }
    }
    Ok(placed)
}

/// What `post` does with a draft, decided before anything is sent from git
/// and the hunks `prepare` or `pull` kept; `--dry-run` prints it. `unsure`
/// says line or file was decided by git's diff of the PR, not GitHub's.
enum Plan {
    /// On its lines at the PR's head, or a reply in its thread.
    Line { placed: Value, unsure: bool },
    /// On the whole file, quoting the lines it has.
    File { placed: Value, unsure: bool },
    /// Left a draft, with why.
    Skip(String),
}

impl Plan {
    /// The protocol's line saying what happens to `draft`.
    fn line(&self, draft: &Value) -> Value {
        let mut out = json!({"id": draft["id"], "path": draft["path"]});
        match self {
            Self::Line { placed, unsure } | Self::File { placed, unsure } => {
                let kind = if matches!(self, Self::Line { .. }) {
                    "line"
                } else {
                    "file"
                };
                out["plan"] = json!(kind);
                let side = placed["side"].as_str().unwrap_or("new");
                if let Some(line) = placed["end"][side].as_u64() {
                    out["line"] = json!(line);
                    out["side"] = json!(side);
                    if *unsure {
                        out["unsure"] = json!(true);
                    }
                }
            }
            Self::Skip(why) => {
                out["plan"] = json!("skip");
                out["reason"] = json!(why);
            }
        }
        json!({ "draft": out })
    }
}

/// Plans a PR's drafts in the comments file's order, so a reply follows
/// the plan of the draft it replies to.
struct Planner {
    /// The session's base and head.
    commits: Option<(String, String)>,
    /// The paths the PR changes, when git could list them.
    changed: Option<HashSet<String>>,
    /// GitHub's hunks of the session's head by path, when they were kept.
    github: HashMap<String, Vec<git::Hunk>>,
    /// git's hunks of the PR by path, for files GitHub's don't cover: read
    /// on the first such file, `None` when git couldn't diff it.
    local: OnceCell<Option<HashMap<String, Vec<git::Hunk>>>>,
    /// Roots with a thread on GitHub, or planned to get one.
    threads: HashSet<String>,
    source: Source,
}

impl Planner {
    fn new(review: &Review, file: &Value) -> Self {
        let commits = review.session().and_then(|s| {
            Some((
                s["base"].as_str()?.to_owned(),
                s["head"].as_str()?.to_owned(),
            ))
        });
        let changed = commits.as_ref().and_then(|(base, head)| {
            let range = format!("{base}...{head}");
            let names = git::git(&["diff", "--name-only", "-z", "--find-renames", &range]).ok()?;
            Some(
                names
                    .split('\0')
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned)
                    .collect(),
            )
        });
        let github = commits
            .as_ref()
            .and_then(|(_, head)| review.hunks(head))
            .unwrap_or_default();
        let threads = comments(file)
            .iter()
            .filter(|c| c["github_thread"].is_string())
            .filter_map(|c| c["id"].as_str().map(str::to_owned))
            .collect();
        Self {
            commits,
            changed,
            github,
            local: OnceCell::new(),
            threads,
            source: Source::default(),
        }
    }

    fn plan(&mut self, draft: &Value) -> Plan {
        if let Some(root) = draft["in_reply_to"].as_str() {
            return if self.threads.contains(root) {
                Plan::Line {
                    placed: draft.clone(),
                    unsure: false,
                }
            } else {
                Plan::Skip("its thread isn't posted".to_owned())
            };
        }
        let path = draft["path"].as_str().unwrap_or_default();
        if self.changed.as_ref().is_some_and(|c| !c.contains(path)) {
            return Plan::Skip("file not in the PR".to_owned());
        }
        let placed = match &self.commits {
            Some((base, head)) => match at_head(draft, (base, head), &mut self.source) {
                Ok(placed) => placed,
                Err(why) => return Plan::Skip(why),
            },
            None => draft.clone(),
        };
        if let Some(id) = draft["id"].as_str() {
            self.threads.insert(id.to_owned());
        }
        let (hunks, unsure) = if let Some(hunks) = self.github.get(path) {
            (Some(hunks.as_slice()), false)
        } else {
            let local = self.local.get_or_init(|| {
                let (base, head) = self.commits.as_ref()?;
                git::hunks(base, head, github::CONTEXT).ok()
            });
            let hunks = local.as_ref();
            (
                hunks.map(|h| h.get(path).map_or(&[][..], Vec::as_slice)),
                true,
            )
        };
        if github::fits_its_lines(&placed, hunks) {
            Plan::Line { placed, unsure }
        } else {
            Plan::File { placed, unsure }
        }
    }
}

/// What `post` posts: your comments not posted yet. One with an author is
/// someone else's, an agent's, and would go out under your name.
fn drafts(file: &Value) -> Vec<&Value> {
    comments(file)
        .iter()
        .filter(|c| c["ref"].is_null() && c["author"].as_str().unwrap_or_default().is_empty())
        .collect()
}

/// Prints each draft's plan, then how many go each way. It reads git, the
/// comments file and the kept hunks, and writes nothing.
fn dry_run(pr: u64) -> Result<(), String> {
    let review = Review::new(pr)?;
    if review.session().is_none() {
        return Err(format!("#{pr} isn't prepared; run prepare {pr} first"));
    }
    let current = read(&review.file())?;
    let mut planner = Planner::new(&review, &current);
    let (mut line, mut file, mut skip) = (0, 0, 0);
    for draft in drafts(&current) {
        let plan = planner.plan(draft);
        *match plan {
            Plan::Line { .. } => &mut line,
            Plan::File { .. } => &mut file,
            Plan::Skip(_) => &mut skip,
        } += 1;
        emit(plan.line(draft));
    }
    emit(json!({"planned": {"line": line, "file": file, "skip": skip}}));
    Ok(())
}

fn post(pr: u64, event: Event, summary: Option<&str>) -> Result<(), String> {
    let summary = read_summary(summary)?;
    let review = Review::new(pr)?;
    let file = review.file();
    let current = read(&file)?;
    let drafts = drafts(&current);
    let state = match github::post_state(&review.remote, pr) {
        Ok(state) => state,
        Err(e) => {
            notice(
                &file,
                Some(&format!("post failed, nothing was posted: {e}")),
            )?;
            return Err(e.into());
        }
    };
    let mut post = Post::new(&review, &file, state, &current);
    post.drafts(&drafts, &mut Planner::new(&review, &current))?;
    if post.stopped.is_none() {
        post.resolves()?;
    }
    // Drafts planned to be left out don't hold the event back: dffy showed
    // them before sending. Refusals nobody saw coming do.
    let held = post.left.iter().any(|l| l.refused) || !post.resolves_refused.is_empty();
    let failure = if let Some(m) = &post.stopped {
        Some(format!(
            "post stopped: {m}; the rest are still drafts, post again to send them"
        ))
    } else if held {
        None
    } else {
        post.send(event, summary.trim_end())
            .err()
            .map(|e| format!("sending the review: {e}"))
    };

    let mut notes = Vec::new();
    if !post.left.is_empty() || held {
        let why: Vec<&str> = (post.left.iter().map(|l| l.note.as_str()))
            .chain(post.resolves_refused.iter().map(String::as_str))
            .collect();
        let pending = if held { ", review left pending" } else { "" };
        notes.push(format!(
            "not posted, still drafts{pending}: {}",
            why.join("; ")
        ));
    }
    notes.extend(failure.clone());
    notice(
        &file,
        (!notes.is_empty()).then(|| notes.join("; ")).as_deref(),
    )?;
    let submitted = matches!(event, Event::Submit(_)) && !held && failure.is_none();
    // A pending review with nothing in it reviewed nothing.
    if failure.is_none() && (post.posted > 0 || submitted) {
        review.reviewed()?;
    }
    for left in &post.left {
        emit(left.line.clone());
    }
    let mut done = json!({
        "posted": post.posted,
        "failed": drafts.len() - post.posted,
        "url": post.url(),
    });
    // `pending` leaves the review pending as asked; only a submit is held.
    if matches!(event, Event::Submit(_)) && !submitted {
        done["held"] = json!(true);
        done["held_message"] = json!(HELD);
    }
    emit(done);
    failure.map_or(Ok(()), Err)
}

/// Submits your pending review, after a `post` left it pending, without
/// posting the drafts still left. With no review pending, the event goes
/// on a new one, as an approval alone does.
fn apply_event(pr: u64, event: Event, summary: Option<&str>) -> Result<(), String> {
    let summary = read_summary(summary)?;
    let review = Review::new(pr)?;
    let file = review.file();
    let state = github::post_state(&review.remote, pr)?;
    let mut post = Post::new(&review, &file, state, &Value::Null);
    post.send(event, summary.trim_end())?;
    // It said the review was left pending.
    notice(&file, None)?;
    review.reviewed()?;
    emit(json!({"posted": 0, "failed": 0, "url": post.url()}));
    Ok(())
}

/// A draft a `post` didn't post.
struct Left {
    /// Its draft line.
    line: Value,
    /// Where it is and why, for the notice.
    note: String,
    /// GitHub refused it, or its thread, while posting; else it was
    /// planned to be left out.
    refused: bool,
}

/// A run of `post`: what it sent, and what it couldn't.
struct Post<'a> {
    file: &'a Path,
    pr_id: String,
    pr_url: String,
    /// The head the comments were made on.
    shown: Option<String>,
    review: Option<github::Review>,
    /// Thread ids by the id of their first comment.
    threads: HashMap<String, String>,
    pace: Pace,
    posted: usize,
    left: Vec<Left>,
    /// Resolves and reopens GitHub refused, with why.
    resolves_refused: Vec<String>,
    /// Why GitHub stopped answering; nothing more is tried.
    stopped: Option<String>,
}

impl<'a> Post<'a> {
    fn new(review: &Review, file: &'a Path, state: github::PostState, current: &Value) -> Self {
        Self {
            file,
            pr_id: state.pr_id,
            pr_url: state.url,
            // The drafts' lines are the session head's, whatever GitHub's is
            // now; dffy warns about a moved head before it posts.
            shown: review
                .session()
                .and_then(|s| s["head"].as_str().map(str::to_owned)),
            review: state.pending,
            threads: comments(current)
                .iter()
                .filter_map(|c| {
                    Some((
                        c["id"].as_str()?.to_owned(),
                        c["github_thread"].as_str()?.to_owned(),
                    ))
                })
                .collect(),
            pace: Pace::default(),
            posted: 0,
            left: Vec::new(),
            resolves_refused: Vec::new(),
            stopped: None,
        }
    }

    /// The review's link, or the PR's when there's no review.
    fn url(&self) -> &str {
        self.review.as_ref().map_or(&self.pr_url, |(_, url)| url)
    }

    fn leave(&mut self, draft: &Value, why: String, refused: bool) {
        self.left.push(Left {
            note: format!("{}: {why}", location(draft)),
            line: Plan::Skip(why).line(draft),
            refused,
        });
    }

    /// Your pending review, started on the first comment that needs it.
    fn review_id(&mut self) -> Result<String, Error> {
        if let Some((id, _)) = &self.review {
            return Ok(id.clone());
        }
        self.pace.wait();
        let review = github::add_review(&self.pr_id, self.shown.as_deref())?;
        Ok(self.review.insert(review).0.clone())
    }

    /// Posts each draft as `planner` plans it, with a progress line for
    /// each, until GitHub stops answering.
    fn drafts(&mut self, drafts: &[&Value], planner: &mut Planner) -> Result<(), String> {
        for (i, draft) in drafts.iter().enumerate() {
            if self.stopped.is_some() {
                break;
            }
            match planner.plan(draft) {
                Plan::Line { placed, .. } => self.draft(&placed, false)?,
                Plan::File { placed, .. } => self.draft(&placed, true)?,
                Plan::Skip(why) => self.leave(draft, why, false),
            }
            emit(json!({"progress": format!("{}/{}", i + 1, drafts.len())}));
        }
        Ok(())
    }

    /// Posts one draft, `on_file` or on its lines, and writes its `ref`
    /// back right away, so a retry skips it. Fails only when that write
    /// does.
    fn draft(&mut self, comment: &Value, on_file: bool) -> Result<(), String> {
        let at = location(comment);
        let thread = if let Some(root) = comment["in_reply_to"].as_str() {
            // Its root was planned, then refused.
            let Some(thread) = self.threads.get(root).cloned() else {
                self.leave(comment, "its thread isn't posted".to_owned(), true);
                return Ok(());
            };
            Some(thread)
        } else {
            None
        };
        // Nothing can post without a review, so failing to start one stops.
        let review = match self.review_id() {
            Ok(review) => review,
            Err(e) => {
                self.stopped = Some(format!("starting a review: {e}"));
                return Ok(());
            }
        };
        self.pace.wait();
        let result = match &thread {
            Some(thread) => github::add_reply(&review, thread, &github::body(comment))
                .map(|posted| (posted, None)),
            None => github::add_thread(&github::thread_variables(&review, comment, on_file))
                .map(|(posted, thread)| (posted, Some(thread))),
        };
        match result {
            Ok((posted, new_thread)) => {
                let id = comment["id"].as_str().unwrap_or_default();
                let reference = format!("github:{}", posted.id);
                let mut fields = json!({"ref": reference, "url": posted.url});
                if let Some(thread) = new_thread {
                    fields["github_thread"] = json!(thread);
                    fields["github_resolved"] = json!(false);
                    self.threads.insert(id.to_owned(), thread);
                }
                apply(
                    self.file,
                    &[
                        json!({"op": "set", "id": id, "fields": {"ref": reference}}),
                        json!({"op": "upsert", "comment": fields}),
                    ],
                )
                .map_err(|e| {
                    format!(
                        "posted {at} but couldn't record it, so a retry would post it again: {e}"
                    )
                })?;
                self.posted += 1;
            }
            Err(Error::Rejected(m)) => self.leave(comment, m, true),
            Err(Error::Failed(m) | Error::Setup(m)) => self.stopped = Some(m),
        }
        Ok(())
    }

    /// Sends the resolves and reopens made in dffy.
    fn resolves(&mut self) -> Result<(), String> {
        // Read again: a thread posted just now may be resolved too.
        for comment in comments(&read(self.file)?).iter().filter(|c| toggled(c)) {
            let to = comment["resolved"] == true;
            let thread = comment["github_thread"].as_str().unwrap_or_default();
            match github::resolve(thread, to) {
                Ok(()) => apply(
                    self.file,
                    &[
                        json!({"op": "upsert", "comment": {"ref": comment["ref"], "github_resolved": to}}),
                    ],
                )?,
                Err(Error::Rejected(m)) => {
                    self.resolves_refused
                        .push(format!("{}: {m}", location(comment)));
                }
                Err(Error::Failed(m) | Error::Setup(m)) => {
                    self.stopped = Some(m);
                    break;
                }
            }
        }
        Ok(())
    }

    /// Submits the review, or sets a pending one's summary. A submit with
    /// no comment in it, as for an approval alone, starts a review first.
    fn send(&mut self, event: Event, summary: &str) -> Result<(), Error> {
        match (event, &self.review) {
            (Event::Submit(event), _) => {
                let review = self.review_id()?;
                github::submit(&review, event, summary)
            }
            (Event::Pending, Some((review, _))) if !summary.is_empty() => {
                github::set_review_body(review, summary)
            }
            (Event::Pending, _) => Ok(()),
        }
    }
}

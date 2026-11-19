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
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::git::{Head, Remote, Source};
use crate::github::{Error, Page};

const USAGE: &str = "usage: dffy-review-github describe | doctor | list | detect | prepare <pr> \
                     | check <pr> | pull <pr> \
                     | post <pr> --event pending|comment|approve|request_changes [--summary-file <file>] \
                     | post <pr> --dry-run [--drafts <file>] \
                     | prefetch | prune [--days <n>]";

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

const VERBS: [&str; 10] = [
    "describe", "doctor", "list", "detect", "prepare", "check", "pull", "post", "prefetch", "prune",
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
        (["prune"], _) => prune(PRUNE_DAYS),
        (["prune", "--days", days], _) => match days.parse() {
            Ok(days) => prune(days),
            Err(_) => return fail(2, &format!("--days takes a number of days, not {days}")),
        },
        (["prepare", _], Some(pr)) => prepare(pr),
        (["check", _], Some(pr)) => check(pr),
        (["pull", _], Some(pr)) => Review::new(pr).and_then(|review| pull(&review)),
        (["post", _, flags @ ..], Some(pr)) => match post_args(flags) {
            Ok((PostMode::Post(event), summary)) => post(pr, event, summary),
            Ok((PostMode::DryRun(drafts), _)) => dry_run(pr, drafts),
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

/// The dffy that ran the plugin writes its comments: it names itself in
/// `DFFY`, which a run by hand doesn't have.
fn dffy_comments(file: &Path) -> Command {
    let dffy = std::env::var_os("DFFY")
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "dffy".into());
    let mut cmd = Command::new(dffy);
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

/// What a session keeps of [`Page::about`] and doesn't print: what
/// `detect` tells the PR of a branch by without asking GitHub, `state`
/// printed lowercase, and when the PR last changed, which `check` compares.
const KEPT: [&str; 5] = [
    "head_ref",
    "state",
    "cross_repository",
    "head_owner",
    "updated_at",
];

/// A PR under review, and what's kept for it: the comments file, drafts
/// included, the session dffy was last given, and GitHub's hunks of a
/// base and head for `post` to plan with. Kept in the data directory, not the state
/// or cache one: the drafts are your writing.
struct Review {
    remote: Remote,
    pr: u64,
    dir: PathBuf,
}

/// Where the PRs of `remote`'s repository are kept, one directory each,
/// beside what's kept of the repository: whether its host is GitHub's
/// (`host.json`), the branch `detect` last found no PR for (`detect.json`)
/// and when `prune` last ran (`pruned`).
fn repository_dir(remote: &Remote) -> Result<PathBuf, String> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok_or("neither XDG_DATA_HOME nor HOME is set")?;
    Ok(data.join("dffy-review-github").join(&remote.key))
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn write_json(dir: &Path, name: &str, value: &Value) -> Result<(), String> {
    let path = dir.join(name);
    // One per process, as nothing locks the directory and a `prepare`
    // and a `post` both save the session. One left by a crash is
    // removed, and `create_new` won't follow a link put in its place.
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    fs::create_dir_all(dir)
        .and_then(|()| {
            fs::remove_file(&tmp).ok();
            fs::File::options()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
        })
        .and_then(|mut file| file.write_all(value.to_string().as_bytes()))
        .and_then(|()| fs::rename(&tmp, &path))
        .map_err(|e| format!("can't write {}: {e}", path.display()))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// How long whether a host is GitHub's is kept.
const HOST_TTL: u64 = 24 * 3600;

/// Whether `remote` is on a GitHub host: github.com, `GH_HOST`, or one gh
/// has a login for. The last is asked of gh once a day, or when gh is
/// replaced or logs in or out, and kept beside the repository's PRs.
fn github_host(remote: &Remote) -> bool {
    let host = remote.host.as_str();
    if host == "github.com" || host == github::default_host() {
        return true;
    }
    let Ok(dir) = repository_dir(remote) else {
        return false;
    };
    let stamp = gh_stamp();
    if let Some(kept) = read_json(&dir.join("host.json")).filter(|k| {
        k["host"] == host
            && k["stamp"] == stamp.as_str()
            && now().saturating_sub(k["at"].as_u64().unwrap_or_default()) < HOST_TTL
    }) {
        return kept["github"] == true;
    }
    let Some(github) = github::logged_in(host) else {
        return false;
    };
    // Unkept, it's asked again next run.
    write_json(
        &dir,
        "host.json",
        &json!({"host": host, "stamp": stamp, "at": now(), "github": github}),
    )
    .ok();
    github
}

/// What changes when gh is replaced, or logs in or out: the modification
/// times and sizes of gh's program on `PATH` and of its hosts file.
fn gh_stamp() -> String {
    let stamp = |path: &Path| {
        fs::metadata(path).ok().map(|m| {
            let at = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            format!("{at}:{}", m.len())
        })
    };
    let program = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .filter(|dir| dir.is_absolute())
            .find_map(|dir| stamp(&dir.join("gh")))
    });
    let config = std::env::var_os("GH_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(|c| PathBuf::from(c).join("gh")))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/gh")));
    let hosts = config.and_then(|c| stamp(&c.join("hosts.yml")));
    format!(
        "{} {}",
        program.unwrap_or_default(),
        hosts.unwrap_or_default()
    )
}

/// The repository's remote on a GitHub host, as [`Remote::pick`] picks
/// it. Every API call of the run goes to its host.
fn remote() -> Result<Remote, String> {
    let remote = Remote::pick(github_host).ok_or(
        "no remote names a GitHub repository: origin, upstream, \
         or the one `gh repo set-default` picked",
    )?;
    if !github_host(&remote) {
        return Err(format!(
            "{}'s host {} isn't github.com, GH_HOST, or a host gh is logged in to",
            remote.git_name, remote.host
        ));
    }
    github::use_host(&remote.host);
    Ok(remote)
}

impl Review {
    fn new(pr: u64) -> Result<Self, String> {
        Self::of(remote()?, pr)
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
    /// with [`KEPT`] as GitHub last said. A session saved before `state`,
    /// `cross_repository` and `head_owner` were kept has none of them, and
    /// is taken as open and the repository's own.
    fn session(&self) -> Option<Value> {
        read_json(&self.dir.join("session.json"))
    }

    /// `session` as dffy reads it: the PR's number as its id, its state
    /// lowercase, with the comments file, without the rest of [`KEPT`].
    fn printed(&self, session: &Value) -> Value {
        let mut session = session.clone();
        let state = session["state"].as_str().map(str::to_lowercase);
        if let Some(fields) = session.as_object_mut() {
            for field in KEPT.iter().chain(&["number", "github_head"]) {
                fields.remove(*field);
            }
        }
        if let Some(state) = state {
            session["state"] = json!(state);
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
        write_json(&self.dir, name, value)
    }

    /// Holds `post.lock` for a run that sends: a second one fails rather
    /// than post the same drafts twice. Released when the file closes.
    fn lock(&self) -> Result<fs::File, String> {
        let path = self.dir.join("post.lock");
        let file = fs::create_dir_all(&self.dir)
            .and_then(|()| {
                fs::File::options()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .mode(0o600)
                    .open(&path)
            })
            .map_err(|e| format!("can't open {}: {e}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(fs::TryLockError::WouldBlock) => {
                Err(format!("already posting #{} in another run", self.pr))
            }
            Err(fs::TryLockError::Error(e)) => Err(format!("can't lock {}: {e}", path.display())),
        }
    }

    /// Keeps GitHub's hunks of the PR from `base` to `head`, for `post` to
    /// plan with, in place of any kept before.
    fn keep_hunks(
        &self,
        base: &str,
        head: &str,
        files: &HashMap<String, Vec<git::Hunk>>,
    ) -> Result<(), String> {
        let files: serde_json::Map<String, Value> = files
            .iter()
            .map(|(path, hunks)| {
                (
                    path.clone(),
                    hunks
                        .iter()
                        .map(|h| json!([h.old.start, h.old.end, h.new.start, h.new.end]))
                        .collect(),
                )
            })
            .collect();
        self.write(
            "hunks.json",
            &json!({"base": base, "head": head, "files": files}),
        )
    }

    /// The base GitHub's kept hunks of `head` were of, empty when that
    /// wasn't kept; `None` without hunks of `head`.
    fn hunks_base(&self, head: &str) -> Option<String> {
        let kept: Value =
            serde_json::from_slice(&fs::read(self.dir.join("hunks.json")).ok()?).ok()?;
        (kept["head"] == head).then(|| kept["base"].as_str().unwrap_or_default().to_owned())
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

    /// Keeps `head`, the PR's head as GitHub just named it, in the session
    /// as `github_head`, for `detect` to tell the branch by once it's
    /// pushed past the session's head, which stays until `pull`.
    fn saw_head(&self, head: &str) -> Result<(), String> {
        match self.session() {
            Some(mut session) if session["head"] != head && session["github_head"] != head => {
                session["github_head"] = json!(head);
                self.save(&session)
            }
            _ => Ok(()),
        }
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

    /// Saves and prints the session on `head` with what it keeps of the PR,
    /// `about` (see [`Page::about`]), keeping the head `post` last
    /// reviewed, else taking `reviewed`'s, your last review's on GitHub.
    fn start(
        &self,
        saved: Option<&Value>,
        (base, head): (&str, &str),
        about: &Value,
        reviewed: Option<&str>,
    ) -> Result<(), String> {
        let mut session = about.clone();
        session["number"] = json!(self.pr);
        session["base"] = json!(base);
        session["head"] = json!(head);
        session["last_reviewed_head"] = saved
            .map(|s| s["last_reviewed_head"].clone())
            .filter(Value::is_string)
            .unwrap_or_else(|| json!(reviewed));
        self.save(&session)?;
        self.emit_session(&session);
        Ok(())
    }
}

/// Writes `page`, and the pages after it as they arrive, into the comments
/// file, threads and general comments, with a progress line for each page
/// of threads. Then fetches the original commits
/// git didn't have, which a force-push left off the PR's head, and places
/// the threads written on them. The fetched commits get no ref: once
/// placed, a thread's lines are in the file, and a re-import that can't
/// read them leaves them. Once every page is written, the comments of
/// `before` GitHub no longer has go, as [`sweep`] says; not after a page
/// that failed, which stops the import, nor when fewer threads came than
/// GitHub counted. Your pending review's comments, every page of them,
/// count as there.
fn import(
    review: &Review,
    mut page: Page,
    source: &mut Source,
    before: &HashSet<String>,
) -> Result<(), String> {
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
            (&mut known.pending, c["pending"] == true),
        ] {
            if has {
                set.insert(reference.to_owned());
            }
        }
    }
    let (mut written, mut waiting, mut seen) = (0, Vec::new(), HashSet::new());
    // Every page asks for the pending review's first comments again.
    let (pending, more_pending) = std::mem::take(&mut page.pending);
    seen.extend(pending.iter().map(|id| format!("github:{id}")));
    if let Some(after) = more_pending {
        let rest = github::rest_of_pending(&review.remote, review.pr, after)?;
        seen.extend(rest.iter().map(|id| format!("github:{id}")));
    }
    let total = page.total;
    // A page after the first may carry only the conversation's lists.
    let mut with_threads = true;
    loop {
        for thread in &mut page.threads {
            github::rest_of_thread(thread)?;
        }
        let ids = page
            .threads
            .iter()
            .flat_map(|t| t["comments"]["nodes"].as_array().into_iter().flatten())
            .filter_map(|c| c["id"].as_str());
        seen.extend(ids.map(|id| format!("github:{id}")));
        let (mut ops, missing) = github::upserts(&page.threads, &page.base, source, &mut known);
        for comment in std::mem::take(&mut page.conversation) {
            seen.extend(comment["ref"].as_str().map(str::to_owned));
            ops.push(json!({"op": "upsert", "comment": comment}));
        }
        if !ops.is_empty() {
            apply(&file, &ops)?;
        }
        waiting.extend(missing);
        written += page.threads.len();
        if with_threads {
            emit(json!({"progress": format!("{written}/{total}")}));
        }
        let Some(after) = page.next.take() else {
            break;
        };
        with_threads = after.threads.is_some();
        page = github::page(&review.remote, review.pr, Some(&after))?;
    }
    if u64::try_from(written) == Ok(total) {
        sweep(&file, before, &seen)?;
    }
    let mut commits: Vec<&str> = waiting.iter().filter_map(github::original_commit).collect();
    commits.sort_unstable();
    commits.dedup();
    // Threads on a commit that didn't arrive stay as they were written.
    if commits.is_empty() || !git::fetch_commits(&review.remote, &commits) {
        return Ok(());
    }
    // A fresh source: the first one keeps its misses.
    let (ops, _) = github::upserts(&waiting, &page.base, &mut Source::default(), &mut known);
    apply(&file, &ops)
}

/// The refs of the comments the file has from GitHub, read before asking
/// GitHub for the first page: a `post` while that's on its way writes a
/// ref the page can't have, and it mustn't look gone. Empty when the file
/// can't be read, which `import`'s own read reports.
fn on_github(file: &Path) -> HashSet<String> {
    read(file)
        .map(|f| {
            comments(&f)
                .iter()
                .filter_map(|c| c["ref"].as_str().filter(|r| r.starts_with("github:")))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Of the comments GitHub had before its first page was asked for, by ref
/// in `before`, removes those it no longer has, not in `seen`, whoever
/// wrote them: only their author can delete a comment or discard a pending
/// review, and posting yours again would undo that. A first comment on a
/// line or a file whose thread still holds replies not posted, or not
/// gone, stays as what they answer, posted no longer, so they aren't lost
/// with it; a general comment goes with its replies.
fn sweep(file: &Path, before: &HashSet<String>, seen: &HashSet<String>) -> Result<(), String> {
    if before.is_subset(seen) {
        return Ok(());
    }
    let current = read(file)?;
    let all = comments(&current);
    let gone = |c: &Value| {
        c["ref"]
            .as_str()
            .is_some_and(|r| before.contains(r) && !seen.contains(r))
    };
    // Removing a first comment removes its thread's replies with it. A
    // general comment goes with replies not posted too: GitHub can't take
    // a reply to one, and without its ref it isn't one the file can hold.
    let removed = |root: &Value| {
        gone(root)
            && (root["path"].as_str().is_none_or(str::is_empty)
                || all
                    .iter()
                    .filter(|c| c["in_reply_to"] == root["id"])
                    .all(gone))
    };
    let mut ops = Vec::new();
    for c in all.iter().filter(|c| gone(c)) {
        if c["in_reply_to"].is_null() && !removed(c) {
            // Without these `post` would take it for posted, or its thread.
            ops.push(
                json!({"op": "upsert", "comment": {"ref": c["ref"], "url": null,
                "outdated": null, "github_thread": null, "github_resolved": null,
                "github_unplaced": null}}),
            );
            ops.push(json!({"op": "set", "id": c["id"], "fields": {"ref": null}}));
        } else if !all
            .iter()
            .any(|root| root["id"] == c["in_reply_to"] && removed(root))
        {
            ops.push(json!({"op": "rm", "id": c["id"]}));
        }
    }
    if ops.is_empty() {
        return Ok(());
    }
    apply(file, &ops)
}

/// GitHub's hunks of the PR, by path.
type Files = HashMap<String, Vec<git::Hunk>>;

/// GitHub's hunks of the PR, asked for in the background.
fn fetch_files(review: &Review) -> JoinHandle<Option<Files>> {
    let (remote, pr) = (review.remote.clone(), review.pr);
    std::thread::spawn(move || github::files(&remote, pr))
}

/// Writes the threads from `page` on, then keeps the hunks `files` gets
/// when they're of the session's head. GitHub doesn't say which base and
/// head its files are of; they're taken as `page`'s, asked for at the same
/// time.
fn import_with_files(
    review: &Review,
    (page, before): (Page, &HashSet<String>),
    files: Option<JoinHandle<Option<Files>>>,
    session_head: &str,
    source: &mut Source,
) -> Result<(), String> {
    let (base, head) = (page.base.clone(), page.head.clone());
    let imported = import(review, page, source, before);
    if let (true, Some(Ok(Some(hunks)))) = (head == session_head, files.map(JoinHandle::join)) {
        review.keep_hunks(&base, &head, &hunks)?;
    }
    imported
}

/// A PR prepared before, with its commits still here, answers from the
/// session at once; the head stays the one dffy shows until `pull`.
fn prepare(pr: u64) -> Result<(), String> {
    let review = Review::new(pr)?;
    let mut source = Source::default();
    // A session without a url was saved with GitHub down, on a guessed base.
    let Some(session) = review.session().filter(|s| {
        s["url"].is_string()
            && [&s["base"], &s["head"]]
                .iter()
                .all(|oid| oid.as_str().is_some_and(|oid| source.has_commit(oid)))
    }) else {
        return pull(&review);
    };
    review.emit_session(&session);
    let head = session["head"].as_str().unwrap_or_default();
    // GitHub's hunks of a base and head don't change; `pull` asks again.
    let kept_base = review.hunks_base(head);
    let mut files = kept_base.is_none().then(|| fetch_files(&review));
    let before = on_github(&review.file());
    let page = github::page(&review.remote, pr, None)?;
    if page.head != head {
        emit(json!({"head": page.head, "moved": true}));
    }
    // Kept for another base: the PR was retargeted, or the one below it
    // merged. Asked for only now, as GitHub's base comes with the page.
    if kept_base.is_some_and(|base| base != page.base) {
        files = Some(fetch_files(&review));
    }
    // Read again, as a `post` may have saved it since.
    if let Some(mut kept) = review.session() {
        let reviewed = page
            .reviewed
            .as_deref()
            .filter(|oid| kept["last_reviewed_head"].is_null() && source.has_commit(oid));
        if reviewed.is_some() || KEPT.iter().any(|&k| kept[k] != page.about[k]) {
            for key in KEPT {
                kept[key] = page.about[key].clone();
            }
            if let Some(oid) = reviewed {
                kept["last_reviewed_head"] = json!(oid);
            }
            review.save(&kept)?;
        }
    }
    review.saw_head(&page.head)?;
    // Threads on a newer head read their anchors from it.
    if !source.has_commit(&page.head) {
        git::fetch_pulls(&review.remote, &[pr])?;
    }
    import_with_files(&review, (page, &before), files, head, &mut source)
}

/// Fetches the head, the first page of threads and the PR's files
/// together, prints the session once both commits are here, then writes
/// every page.
fn pull(review: &Review) -> Result<(), String> {
    let pr = review.pr;
    let saved = review.session();
    let files = fetch_files(review);
    let before = on_github(&review.file());
    let (fetched, first) = std::thread::scope(|s| {
        let fetch = s.spawn(|| git::fetch_pulls(&review.remote, &[pr]));
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
                None => git::git(&[
                    "rev-parse",
                    &format!("refs/remotes/{}/HEAD", review.remote.git_name),
                ])?,
            };
            review.start(
                saved.as_ref(),
                (&base, &head),
                saved.as_ref().unwrap_or(&Value::Null),
                None,
            )?;
            return Err(format!("couldn't reach GitHub, comments may be stale: {e}"));
        }
    };
    let mut source = Source::default();
    if !source.has_commit(&page.head) {
        fetched?;
        return Err(format!(
            "{}'s refs/pull/{pr}/head isn't the PR's head {}; try again",
            review.remote.git_name, page.head
        ));
    }
    if !source.has_commit(&page.base) {
        git::git(&[
            "fetch",
            "--quiet",
            "--no-tags",
            "--end-of-options",
            &review.remote.git_name,
            &page.base,
        ])?;
    }
    // "Since last review" diffs from it, so it has to be here.
    let reviewed = page
        .reviewed
        .as_deref()
        .filter(|oid| source.has_commit(oid));
    review.start(
        saved.as_ref(),
        (&page.base, &page.head),
        &page.about,
        reviewed,
    )?;
    let head = page.head.clone();
    import_with_files(review, (page, &before), Some(files), &head, &mut source)
}

/// Whether the PR's head on GitHub is still the session's, and whether the
/// PR changed since it was last imported, which new comments change it
/// by. Asked with the `ETag` of the last answer, kept in `check.json`:
/// GitHub doesn't count an unchanged answer against the rate limit.
fn check(pr: u64) -> Result<(), String> {
    let review = Review::new(pr)?;
    let session = review
        .session()
        .ok_or_else(|| format!("#{pr} isn't prepared; run prepare {pr} first"))?;
    let kept = read_json(&review.dir.join("check.json"));
    let etag = kept.as_ref().and_then(|k| k["etag"].as_str());
    let now = if let Some(now) = github::pull(&review.remote, pr, etag)? {
        let now = json!({"etag": now.etag, "head": now.head, "updated_at": now.updated_at});
        review.write("check.json", &now)?;
        now
    } else {
        kept.unwrap_or_default()
    };
    let head = now["head"].as_str().unwrap_or_default();
    if !head.is_empty() {
        review.saw_head(head)?;
    }
    emit(json!({
        "head": head,
        "moved": session["head"] != head,
        "comments_moved": session["updated_at"].as_str().is_some_and(|u| now["updated_at"] != u),
    }));
    Ok(())
}

/// How long `detect` keeps having found no PR for a branch at a commit.
const NO_PR_TTL: u64 = 600;

/// The open PR the branch checked out is the head of, for plain `dffy`, as
/// [`Head::of`] tells it. From a session kept for it when there's one that
/// GitHub last said is open, which needs nothing from GitHub, else from
/// one query. `{"review": null}` without one, on a detached HEAD, or
/// without a remote on a GitHub host, which asks GitHub nothing; nor does
/// a branch GitHub had no PR for in the last 10 minutes, at the same
/// commit. Then sessions of PRs long closed go, at most once a day.
fn detect() -> Result<(), String> {
    let Ok(branch) = git::git(&["symbolic-ref", "--quiet", "--short", "HEAD"]) else {
        emit(json!({ "review": null }));
        return Ok(());
    };
    let Ok(remote) = remote() else {
        emit(json!({ "review": null }));
        return Ok(());
    };
    let found = detected(&remote, &branch)?;
    emit(json!({ "review": found }));
    prune_daily(&remote);
    Ok(())
}

fn detected(remote: &Remote, branch: &str) -> Result<Value, String> {
    let head = Head::of(branch, remote);
    let kept: Vec<u64> = if let Head::Pull(pr) = head {
        vec![pr]
    } else {
        let mut kept: Vec<u64> = fs::read_dir(repository_dir(remote)?)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect();
        // A branch's newest PR first: older ones on it are likely closed.
        kept.sort_unstable_by(|a, b| b.cmp(a));
        kept
    };
    let is_ancestor = |a: &str, b: &str| git::git(&["merge-base", "--is-ancestor", a, b]).is_ok();
    let remotes = format!("refs/remotes/{}/", remote.git_name);
    let default_branch = format!("{remotes}HEAD");
    // In the default branch's history; without one known, as a clone made
    // by `git remote add` has none, in any of the remote's branches but
    // the PR's own.
    let merged = |kept: &str, own: &str| {
        if git::git(&["rev-parse", "--verify", "--quiet", &default_branch]).is_ok() {
            return is_ancestor(kept, &default_branch);
        }
        let own = format!("{remotes}{own}");
        git::git(&[
            "for-each-ref",
            "--format=%(refname)",
            "--contains",
            kept,
            &remotes,
        ])
        .is_ok_and(|refs| refs.lines().any(|r| r != own))
    };
    // A branch cut again under the name after the PR merged isn't the
    // PR's: the branch holds the PR's head, the session's or the one
    // GitHub last named, when either is in the other's history and it
    // isn't merged. Not caught without asking GitHub: the same branch
    // carried on after a squash, or a merge into another base.
    let ours = |s: &Value| match &head {
        Head::Pull(_) => true,
        Head::Branch { name, .. } => {
            s["head_ref"] == name.as_str()
                && head.owns(&s["cross_repository"], &s["head_owner"])
                && [&s["head"], &s["github_head"]]
                    .into_iter()
                    .filter_map(Value::as_str)
                    .any(|kept| {
                        (is_ancestor(kept, "HEAD") || is_ancestor("HEAD", kept))
                            && !merged(kept, name)
                    })
        }
    };
    for pr in kept {
        let review = Review::of(remote.clone(), pr)?;
        if let Some(session) = review
            .session()
            .filter(|s| s["state"].as_str().is_none_or(|s| s == "OPEN") && ours(s))
        {
            return Ok(review.printed(&session));
        }
    }
    let dir = repository_dir(remote)?;
    let none = json!({
        "branch": branch,
        "commit": git::git(&["rev-parse", "--verify", "--quiet", "HEAD"]).ok(),
    });
    if let Some(kept) = read_json(&dir.join("detect.json"))
        && kept["branch"] == none["branch"]
        && kept["commit"] == none["commit"]
        && now().saturating_sub(kept["at"].as_u64().unwrap_or_default()) < NO_PR_TTL
    {
        return Ok(Value::Null);
    }
    let Some(mut found) = github::detect(remote, &head)? else {
        let mut none = none;
        none["at"] = json!(now());
        write_json(&dir, "detect.json", &none)?;
        return Ok(Value::Null);
    };
    let pr = found["number"].as_u64().unwrap_or_default();
    let review = Review::of(remote.clone(), pr)?;
    if let Some(head) = found["head"].as_str() {
        review.saw_head(head)?;
    }
    found["last_reviewed_head"] = review
        .session()
        .map_or(Value::Null, |s| s["last_reviewed_head"].clone());
    Ok(review.printed(&found))
}

fn open_prs() -> Result<(Remote, Vec<Value>), String> {
    let remote = remote()?;
    let head = git::git(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|branch| Head::of(&branch, &remote));
    let prs = github::list(&remote, head.as_ref())?;
    Ok((remote, prs))
}

/// The PRs to pick from, each with how many of your drafts its session's
/// comments file holds, when there are any.
fn list() -> Result<(), String> {
    let (remote, mut prs) = open_prs()?;
    for pr in &mut prs {
        let Some(review) = pr["id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .and_then(|n| Review::of(remote.clone(), n).ok())
            .filter(|r| r.file().exists())
        else {
            continue;
        };
        let count = read(&review.file()).map_or(0, |f| drafts(&f).len());
        if count > 0 {
            pr["drafts"] = json!(count);
        }
    }
    emit(json!({ "reviews": prs }));
    prune_daily(&remote);
    Ok(())
}

/// Fetches the heads of the PRs `list` shows, so preparing one later only
/// has to ask GitHub for its threads.
fn prefetch() -> Result<(), String> {
    let (remote, prs) = open_prs()?;
    let prs: Vec<u64> = prs
        .iter()
        .filter_map(|pr| pr["id"].as_str()?.parse().ok())
        .collect();
    if !prs.is_empty() {
        git::fetch_pulls(&remote, &prs)?;
    }
    emit(json!({ "fetched": prs }));
    Ok(())
}

/// Days a closed or merged PR's session is kept untouched before `prune`
/// drops it.
const PRUNE_DAYS: u64 = 30;

/// Drops the sessions of PRs GitHub last said were closed or merged, not
/// prepared or pulled for `days` days, with their `refs/dffy/pull/<pr>`,
/// and prints `{"pruned":[…]}`. One whose comments file holds anything not
/// on GitHub stays: a comment without a `ref`, yours or an agent's, or a
/// resolve not sent.
fn prune(days: u64) -> Result<(), String> {
    let pruned = prune_sessions(&remote()?, days)?;
    emit(json!({ "pruned": pruned }));
    Ok(())
}

fn prune_sessions(remote: &Remote, days: u64) -> Result<Vec<u64>, String> {
    let dir = repository_dir(remote)?;
    let old = Duration::from_secs(days.saturating_mul(86_400));
    let mut pruned = Vec::new();
    for pr in fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u64>().ok())
    {
        let review = Review::of(remote.clone(), pr)?;
        let session = review.dir.join("session.json");
        let untouched = fs::metadata(&session)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= old);
        let closed =
            read_json(&session).is_some_and(|s| s["state"] == "CLOSED" || s["state"] == "MERGED");
        if !untouched || !closed {
            continue;
        }
        let unsent = |c: &Value| c["ref"].is_null() || toggled(c);
        if review.file().exists()
            && !read(&review.file()).is_ok_and(|f| !comments(&f).iter().any(unsent))
        {
            continue;
        }
        fs::remove_dir_all(&review.dir)
            .map_err(|e| format!("can't remove {}: {e}", review.dir.display()))?;
        // Already gone when it was never fetched.
        git::git(&["update-ref", "-d", &git::pull_ref(pr)]).ok();
        pruned.push(pr);
    }
    pruned.sort_unstable();
    Ok(pruned)
}

/// [`prune_sessions`] with [`PRUNE_DAYS`], when it hasn't run in a day,
/// after `list` or `detect` printed their line. What fails is left for
/// the next day.
fn prune_daily(remote: &Remote) {
    let Ok(dir) = repository_dir(remote) else {
        return;
    };
    let stamp = dir.join("pruned");
    let recent = fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(86_400));
    if recent || !dir.is_dir() {
        return;
    }
    if fs::write(&stamp, "").is_ok() {
        prune_sessions(remote, PRUNE_DAYS).ok();
    }
}

/// For `dffy doctor`: whether gh is installed, and logged in to the host
/// the API calls go to, the repository's, else gh's own. `gh auth status`
/// asks that host, so it isn't free; no other verb runs it for a host
/// known to be GitHub's, they read the same from their first call.
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
            let host = Remote::pick(github_host).map_or_else(github::default_host, |r| r.host);
            github::use_host(&host);
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

enum PostMode<'a> {
    Post(Event),
    /// Says what `post` would do with each draft, without asking GitHub,
    /// and with those of another comments file.
    DryRun(Option<&'a str>),
}

/// `--dry-run` alone or with `--drafts`, or `--event` and `--summary-file`
/// in either order.
fn post_args<'a>(flags: &[&'a str]) -> Result<(PostMode<'a>, Option<&'a str>), String> {
    match flags {
        ["--dry-run"] => return Ok((PostMode::DryRun(None), None)),
        ["--dry-run", "--drafts", file] | ["--drafts", file, "--dry-run"] => {
            return Ok((PostMode::DryRun(Some(file)), None));
        }
        _ => {}
    }
    let (mut event, mut summary) = (None, None);
    let mut flags = flags.iter();
    while let Some(flag) = flags.next() {
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
    let event = event.ok_or(USAGE)?;
    Ok((PostMode::Post(event), summary))
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
    // A range from the other side is anchored by its last line alone, so
    // nothing says where its start went: it goes on that line.
    if delta != 0 && placed["start_side"].as_str().is_some_and(|s| s != side) {
        if let Some(fields) = placed.as_object_mut() {
            fields.remove("start_side");
        }
        placed["start"] = json!({ side: placed["end"][side] });
    }
    Ok(placed)
}

/// What `post` does with a draft, decided from git and the hunks `prepare`
/// or `pull` kept before anything is sent; `--dry-run` prints it. `unsure`
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
    /// The paths the PR changes, when git could list them: read on the
    /// first draft that isn't a reply.
    changed: OnceCell<Option<HashSet<String>>>,
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
            changed: OnceCell::new(),
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
        let changed = self.changed.get_or_init(|| {
            let (base, head) = self.commits.as_ref()?;
            let range = format!("{base}...{head}");
            let names = git::git(&[
                "diff",
                "--name-only",
                "-z",
                "--find-renames",
                "--end-of-options",
                &range,
                "--",
            ])
            .ok()?;
            Some(
                names
                    .split('\0')
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned)
                    .collect(),
            )
        });
        if changed.as_ref().is_some_and(|c| !c.contains(path)) {
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
            (
                local
                    .as_ref()
                    .map(|h| h.get(path).map_or(&[][..], Vec::as_slice)),
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

/// Prints each draft's plan, then how many go each way, how many comments
/// are in your pending review on GitHub already, as last imported, and how
/// many resolves and reopens wait to be sent. The drafts of `extra`, a
/// comments file of drafts not moved into the PR's yet, are planned after
/// the PR's own. It reads git, the comments files and the kept hunks, and
/// writes nothing.
fn dry_run(pr: u64, extra: Option<&str>) -> Result<(), String> {
    let review = Review::new(pr)?;
    if review.session().is_none() {
        return Err(format!("#{pr} isn't prepared; run prepare {pr} first"));
    }
    let current = read(&review.file())?;
    let extra = extra.map(|f| read(Path::new(f))).transpose()?;
    let mut planner = Planner::new(&review, &current);
    let (mut line, mut file, mut skip) = (0, 0, 0);
    let all = drafts(&current)
        .into_iter()
        .chain(extra.as_ref().map(drafts).unwrap_or_default());
    for draft in all {
        let plan = planner.plan(draft);
        *match plan {
            Plan::Line { .. } => &mut line,
            Plan::File { .. } => &mut file,
            Plan::Skip(_) => &mut skip,
        } += 1;
        emit(plan.line(draft));
    }
    let count = |has: fn(&Value) -> bool| comments(&current).iter().filter(|c| has(c)).count();
    emit(json!({"planned": {"line": line, "file": file, "skip": skip,
        "pending": count(|c| c["pending"] == true), "resolves": count(toggled)}}));
    Ok(())
}

/// Posts the drafts into your pending review, sends the resolves and
/// reopens, then applies `event`. Drafts left out, planned so or refused by
/// GitHub, don't hold the event back; one GitHub refused says why in its
/// `post_error`. Only a stop, GitHub no longer answering, does: the rest
/// are posted by the next run.
fn post(pr: u64, event: Event, summary: Option<&str>) -> Result<(), String> {
    let summary = read_summary(summary)?;
    let review = Review::new(pr)?;
    let _lock = review.lock()?;
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
    let failure = if let Some(m) = &post.stopped {
        Some(format!(
            "post stopped: {m}; the rest are still drafts, post again to send them"
        ))
    } else {
        post.send(event, summary.trim_end())
            .err()
            .map(|e| format!("sending the review: {e}"))
    };
    let mut notes: Vec<String> = post
        .resolves_refused
        .iter()
        .map(|why| format!("not resolved or reopened: {why}"))
        .collect();
    notes.extend(failure.clone());
    notice(
        &file,
        (!notes.is_empty()).then(|| notes.join("; ")).as_deref(),
    )?;
    let submitted = matches!(event, Event::Submit(_)) && failure.is_none();
    if submitted {
        post.submitted()?;
    }
    // A pending review with nothing in it reviewed nothing.
    if failure.is_none() && (post.posted > 0 || submitted) {
        review.reviewed()?;
    }
    for left in &post.left {
        emit(left.clone());
    }
    emit(json!({
        "posted": post.posted,
        "failed": drafts.len() - post.posted,
        "pending": post.pending_before,
        "url": post.url(),
    }));
    failure.map_or(Ok(()), Err)
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
    /// The comments your pending review had before this run.
    pending_before: u64,
    /// The draft lines of the drafts not posted.
    left: Vec<Value>,
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
            pending_before: state.pending_comments,
            left: Vec::new(),
            resolves_refused: Vec::new(),
            stopped: None,
        }
    }

    /// The review's link, or the PR's when there's no review.
    fn url(&self) -> &str {
        self.review.as_ref().map_or(&self.pr_url, |(_, url)| url)
    }

    fn leave(&mut self, draft: &Value, why: String) {
        self.left.push(Plan::Skip(why).line(draft));
    }

    /// Clears `pending` once the review it marked is submitted.
    fn submitted(&self) -> Result<(), String> {
        let ops: Vec<Value> = comments(&read(self.file)?)
            .iter()
            .filter(|c| c["pending"] == true)
            .map(|c| json!({"op": "upsert", "comment": {"ref": c["ref"], "pending": null}}))
            .collect();
        if ops.is_empty() {
            return Ok(());
        }
        apply(self.file, &ops)
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
                Plan::Skip(why) => self.leave(draft, why),
            }
            emit(json!({"progress": format!("{}/{}", i + 1, drafts.len())}));
        }
        Ok(())
    }

    /// Posts one draft, `on_file` or on its lines, and writes its `ref`
    /// back right away, so a retry skips it, marked `pending` in your
    /// pending review until that's submitted. One GitHub refuses gets why
    /// in its `post_error`, which goes once it's posted. Fails only when a
    /// write does.
    fn draft(&mut self, comment: &Value, on_file: bool) -> Result<(), String> {
        let id = comment["id"].as_str().unwrap_or_default();
        let thread = if let Some(root) = comment["in_reply_to"].as_str() {
            // Its root was planned, then refused.
            let Some(thread) = self.threads.get(root).cloned() else {
                self.leave(comment, "its thread isn't posted".to_owned());
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
                let reference = format!("github:{}", posted.id);
                let mut fields = json!({"ref": reference, "url": posted.url, "pending": true});
                if let Some(thread) = new_thread {
                    fields["github_thread"] = json!(thread);
                    fields["github_resolved"] = json!(false);
                    self.threads.insert(id.to_owned(), thread);
                }
                apply(
                    self.file,
                    &[
                        json!({"op": "set", "id": id,
                            "fields": {"ref": reference, "post_error": null}}),
                        json!({"op": "upsert", "comment": fields}),
                    ],
                )
                .map_err(|e| {
                    format!(
                        "posted {} but couldn't record it, so a retry would post it again: {e}",
                        location(comment)
                    )
                })?;
                self.posted += 1;
            }
            Err(Error::Rejected(m)) => {
                apply(
                    self.file,
                    &[json!({"op": "set", "id": id, "fields": {"post_error": m}})],
                )?;
                self.leave(comment, m);
            }
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
    /// no comment in it, as for an approval alone, or a summary with none,
    /// starts a review first.
    fn send(&mut self, event: Event, summary: &str) -> Result<(), Error> {
        match event {
            Event::Submit(event) => github::submit(&self.review_id()?, event, summary),
            Event::Pending if !summary.is_empty() => {
                github::set_review_body(&self.review_id()?, summary)
            }
            Event::Pending => Ok(()),
        }
    }
}

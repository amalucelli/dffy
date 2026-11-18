//! GitHub through `gh api`, which owns auth, tokens and hosts: the GraphQL
//! queries and mutations, the mapping between GitHub's review threads and
//! the comments file, and the PR's diff as GitHub has it.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Write as _;
use std::process::{Command, Stdio};

use serde_json::{Map, Value, json};

use crate::git::{self, Remote, Source};

/// Threads a page; at 100 comments each it stays under GitHub's node limit.
const THREADS_PER_PAGE: u32 = 100;

/// Hosts accept a line comment only this close to a change.
pub(crate) const CONTEXT: u64 = 3;

const PULL_REQUEST: &str = "query PullRequest($owner: String!, $name: String!, $number: Int!, $after: String, $first: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      title url baseRefOid headRefOid headRefName
      reviewThreads(first: $first, after: $after) {
        totalCount
        pageInfo { hasNextPage endCursor }
        nodes {
          id isResolved isOutdated path diffSide startDiffSide originalLine originalStartLine subjectType
          comments(first: 100) {
            nodes { id url body createdAt viewerDidAuthor author { login } originalCommit { oid } }
          }
        }
      }
    }
  }
}";

/// Pending reviews are visible only to their author, so the first one is
/// yours.
const POST_STATE: &str = "query PostState($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      id url
      reviews(states: [PENDING], first: 1) { nodes { id url } }
    }
  }
}";

const ADD_REVIEW: &str = "mutation AddReview($pr: ID!, $commit: GitObjectID) {
  addPullRequestReview(input: {pullRequestId: $pr, commitOID: $commit}) { pullRequestReview { id url } }
}";

const ADD_THREAD: &str = "mutation AddPullRequestReviewThread($review: ID!, $path: String!, $body: String!, $line: Int, $side: DiffSide, $startLine: Int, $startSide: DiffSide, $subjectType: PullRequestReviewThreadSubjectType) {
  addPullRequestReviewThread(input: {pullRequestReviewId: $review, path: $path, body: $body, line: $line, side: $side, startLine: $startLine, startSide: $startSide, subjectType: $subjectType}) {
    thread { id comments(first: 1) { nodes { id url } } }
  }
}";

const ADD_REPLY: &str = "mutation AddPullRequestReviewThreadReply($review: ID!, $thread: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: {pullRequestReviewId: $review, pullRequestReviewThreadId: $thread, body: $body}) { comment { id url } }
}";

const RESOLVE: &str = "mutation ResolveReviewThread($thread: ID!) {
  resolveReviewThread(input: {threadId: $thread}) { thread { id isResolved } }
}";

const UNRESOLVE: &str = "mutation UnresolveReviewThread($thread: ID!) {
  unresolveReviewThread(input: {threadId: $thread}) { thread { id isResolved } }
}";

const SUBMIT: &str = "mutation SubmitPullRequestReview($review: ID!, $event: PullRequestReviewEvent!, $body: String) {
  submitPullRequestReview(input: {pullRequestReviewId: $review, event: $event, body: $body}) { pullRequestReview { id } }
}";

const UPDATE_REVIEW: &str = "mutation UpdatePullRequestReview($review: ID!, $body: String!) {
  updatePullRequestReview(input: {pullRequestReviewId: $review, body: $body}) { pullRequestReview { id } }
}";

/// Without a branch, `@include` leaves out the current branch's PRs rather
/// than asking for every PR.
const LIST: &str = "query List($owner: String!, $name: String!, $requested: String!, $mine: String!, $branch: String!, $onBranch: Boolean!) {
  requested: search(query: $requested, type: ISSUE, first: 50) { nodes { ...Listed } }
  mine: search(query: $mine, type: ISSUE, first: 50) { nodes { ...Listed } }
  repository(owner: $owner, name: $name) @include(if: $onBranch) {
    pullRequests(headRefName: $branch, states: OPEN, first: 5) { nodes { ...Listed } }
  }
}
fragment Listed on PullRequest { number title url headRefName author { login } }";

const DETECT: &str = "query Detect($owner: String!, $name: String!, $branch: String!) {
  repository(owner: $owner, name: $name) {
    pullRequests(headRefName: $branch, states: OPEN, first: 1) { nodes { number title url baseRefOid headRefOid } }
  }
}";

pub(crate) const NO_GH: &str =
    "dffy-review-github needs the GitHub CLI (gh): https://cli.github.com";

/// The host gh sends API calls to: `GH_HOST`, else github.com. No call
/// passes `--hostname`, so this is the one its login has to be for.
pub(crate) fn host() -> String {
    std::env::var("GH_HOST")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "github.com".to_owned())
}

pub(crate) fn logged_out() -> String {
    format!("gh isn't logged in to {}; run `gh auth login`", host())
}

#[derive(Debug)]
pub(crate) enum Error {
    /// GitHub answered and refused this request; others may still work.
    Rejected(String),
    /// No answer, or a rate limit: stop.
    Failed(String),
    /// gh isn't installed or isn't logged in: no request can work.
    Setup(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(m) | Self::Failed(m) | Self::Setup(m) => f.write_str(m),
        }
    }
}

impl From<Error> for String {
    fn from(e: Error) -> Self {
        e.to_string()
    }
}

/// Runs one named operation. GraphQL errors in the answer are a rejection
/// unless they're a rate limit; no answer at all is a failure.
fn graphql(operation: &str, query: &str, variables: &Value) -> Result<Value, Error> {
    let body = json!({"query": query, "operationName": operation, "variables": variables});
    let mut child = Command::new("gh")
        .args(["api", "graphql", "--input", "-"])
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::Setup(NO_GH.to_owned()),
            _ => Error::Failed(format!("can't run gh: {e}")),
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(body.to_string().as_bytes())
            .map_err(|e| Error::Failed(format!("can't write to gh: {e}")))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| Error::Failed(format!("gh: {e}")))?;
    let answer: Option<Value> = serde_json::from_slice(&out.stdout).ok();
    if let Some(errors) = answer
        .as_ref()
        .and_then(|a| a.get("errors"))
        .and_then(Value::as_array)
        .filter(|e| !e.is_empty())
    {
        let message = errors
            .iter()
            .filter_map(|e| e["message"].as_str())
            .collect::<Vec<_>>()
            .join("; ");
        let limited = errors.iter().any(|e| e["type"] == "RATE_LIMITED")
            || message.to_lowercase().contains("rate limit");
        return Err(if limited {
            Error::Failed(message)
        } else {
            Error::Rejected(message)
        });
    }
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr = stderr.trim();
        // gh exits 4 without a token; a revoked or expired one gets a 401.
        if out.status.code() == Some(4) || stderr.contains("HTTP 401") {
            return Err(Error::Setup(logged_out()));
        }
        return Err(Error::Failed(if stderr.is_empty() {
            format!("gh exited with {}", out.status)
        } else {
            stderr.to_owned()
        }));
    }
    answer
        .and_then(|mut a| a.get_mut("data").map(Value::take))
        .ok_or_else(|| Error::Failed("gh's answer has no data".to_owned()))
}

fn pull_request(data: &Value, pr: u64) -> Result<&Value, Error> {
    let found = &data["repository"]["pullRequest"];
    if found.is_object() {
        Ok(found)
    } else {
        Err(Error::Rejected(format!("no pull request #{pr}")))
    }
}

fn string(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// A page of a PR's review threads.
pub(crate) struct Page {
    pub title: Value,
    pub url: Value,
    pub base: String,
    pub head: String,
    /// The PR's branch.
    pub head_ref: Value,
    pub threads: Vec<Value>,
    /// The PR's threads, on every page.
    pub total: u64,
    pub next: Option<String>,
}

pub(crate) fn page(remote: &Remote, pr: u64, after: Option<&str>) -> Result<Page, Error> {
    let data = graphql(
        "PullRequest",
        PULL_REQUEST,
        &json!({"owner": remote.owner, "name": remote.name, "number": pr, "after": after, "first": THREADS_PER_PAGE}),
    )?;
    let found = pull_request(&data, pr)?;
    let threads = &found["reviewThreads"];
    Ok(Page {
        title: found["title"].clone(),
        url: found["url"].clone(),
        base: string(&found["baseRefOid"]),
        head: string(&found["headRefOid"]),
        head_ref: found["headRefName"].clone(),
        threads: threads["nodes"].as_array().cloned().unwrap_or_default(),
        total: threads["totalCount"].as_u64().unwrap_or_default(),
        next: threads["pageInfo"]["endCursor"]
            .as_str()
            .filter(|_| threads["pageInfo"]["hasNextPage"] == true)
            .map(str::to_owned),
    })
}

/// GitHub's own diff of a PR, as hunks by path: the lines it takes line
/// comments on, which a local `git diff` can place differently at a hunk's
/// edges. A file GitHub lists without a patch has no hunks when nothing
/// in it changed line by line (binary, or renamed only), and is left out
/// when its patch was too large; so are the files past the 3000 GitHub
/// lists. `None` when gh failed.
pub(crate) fn files(remote: &Remote, pr: u64) -> Option<HashMap<String, Vec<git::Hunk>>> {
    let out = Command::new("gh")
        .args([
            "api",
            &format!(
                "repos/{}/{}/pulls/{pr}/files?per_page=100",
                remote.owner, remote.name
            ),
            "--paginate",
        ])
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse_files(&out.stdout))
        .flatten()
}

/// Read as a stream of arrays: `gh api --paginate` merges the pages into
/// one array, or, by version, prints each page's after the last.
fn parse_files(pages: &[u8]) -> Option<HashMap<String, Vec<git::Hunk>>> {
    let mut files = HashMap::new();
    for page in serde_json::Deserializer::from_slice(pages).into_iter::<Vec<Value>>() {
        for file in page.ok()? {
            let Some(name) = file["filename"].as_str() else {
                continue;
            };
            let hunks = match file["patch"].as_str() {
                Some(patch) => git::patch_hunks(patch),
                None if file["changes"] == 0 => Vec::new(),
                None => continue,
            };
            files.insert(name.to_owned(), hunks);
        }
    }
    Some(files)
}

/// Why a line thread is a file comment, in its `github_unplaced`, when it's
/// its original commit that git doesn't have.
pub(crate) const UNPLACED: &str = "original commit not available";

/// What the comments file holds of imported threads, by their first
/// comment's ref.
#[derive(Default)]
pub(crate) struct Known {
    /// Resolved or reopened here and not posted yet: `resolved` stays.
    pub toggled: HashSet<String>,
    /// On their lines already. A re-import that can't read them leaves
    /// them there, so their original commit is never needed again.
    pub anchored: HashSet<String>,
    /// With a `github_unplaced`, which goes once it no longer holds.
    pub unplaced: HashSet<String>,
}

/// The comments file's operations for a page of threads: each thread's
/// first comment, with its lines and an anchor read from git, then its
/// replies, and the threads whose original commit git doesn't have.
/// A thread whose lines git can't read comes in as a file comment, and
/// says why when that's its missing commit.
pub(crate) fn upserts(
    threads: &[Value],
    base: &str,
    source: &mut Source,
    known: &mut Known,
) -> (Vec<Value>, Vec<Value>) {
    let (mut ops, mut waiting) = (Vec::new(), Vec::new());
    for thread in threads {
        let Some((root, replies)) = thread["comments"]["nodes"]
            .as_array()
            .and_then(|c| c.split_first())
        else {
            continue;
        };
        let root_ref = format!("github:{}", string(&root["id"]));
        let mut comment = imported(root, &root_ref);
        comment.insert("path".to_owned(), thread["path"].clone());
        comment.insert("github_thread".to_owned(), thread["id"].clone());
        comment.insert("github_resolved".to_owned(), thread["isResolved"].clone());
        if !known.toggled.contains(&root_ref) {
            comment.insert("resolved".to_owned(), thread["isResolved"].clone());
        }
        // Only when true: an upsert that adds a comment keeps a null field.
        if thread["isOutdated"] == true {
            comment.insert("outdated".to_owned(), json!(true));
        }
        let missing = match placed(thread, base, source) {
            Some(lines) => {
                comment.extend(lines);
                false
            }
            None => {
                !known.anchored.contains(&root_ref)
                    && thread["subjectType"] == "LINE"
                    && original_commit(thread).is_some_and(|oid| !source.has_commit(oid))
            }
        };
        if missing {
            comment.insert("github_unplaced".to_owned(), json!(UNPLACED));
            known.unplaced.insert(root_ref.clone());
            waiting.push(thread.clone());
        } else if known.unplaced.remove(&root_ref) {
            comment.insert("github_unplaced".to_owned(), Value::Null);
        }
        ops.push(json!({"op": "upsert", "comment": comment}));
        for reply in replies {
            let mut comment = imported(reply, &format!("github:{}", string(&reply["id"])));
            comment.insert("in_reply_to_ref".to_owned(), json!(root_ref));
            ops.push(json!({"op": "upsert", "comment": comment}));
        }
    }
    (ops, waiting)
}

/// The commit a thread was written on: its first comment's.
pub(crate) fn original_commit(thread: &Value) -> Option<&str> {
    thread["comments"]["nodes"][0]["originalCommit"]["oid"].as_str()
}

fn imported(node: &Value, reference: &str) -> Map<String, Value> {
    let (text, suggestion) = split_suggestion(&string(&node["body"]).replace("\r\n", "\n"));
    let author = if node["viewerDidAuthor"] == true {
        ""
    } else {
        // GitHub's name for a deleted account.
        node["author"]["login"].as_str().unwrap_or("ghost")
    };
    [
        ("ref", json!(reference)),
        ("url", node["url"].clone()),
        ("text", json!(text)),
        ("suggestion", json!(suggestion)),
        ("author", json!(author)),
        ("created", node["createdAt"].clone()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}

/// A line thread's side, lines and anchor, from the lines it was written on
/// in its `originalCommit`, outdated or not: GitHub's current lines are its
/// own placement on a later head, which dffy redoes from the anchor's text.
/// An old-side line is in that commit's merge base with the PR's base.
fn placed(thread: &Value, base: &str, source: &mut Source) -> Option<Map<String, Value>> {
    if thread["subjectType"] != "LINE" {
        return None;
    }
    let line = thread["originalLine"].as_u64()?;
    let commit = original_commit(thread)?;
    let side = side_name(&thread["diffSide"])?;
    let start_side = side_name(&thread["startDiffSide"]).unwrap_or(side);
    let start = thread["originalStartLine"].as_u64().unwrap_or(line);
    let at = if side == "old" {
        source.merge_base(base, commit)?
    } else {
        commit.to_owned()
    };
    let lines = source.lines(&at, thread["path"].as_str()?)?;
    // A range starting on the other side is anchored by its last line.
    let first = if start_side == side { start } else { line };
    let anchor = git::anchor(
        lines,
        usize::try_from(first).ok()?,
        usize::try_from(line).ok()?,
    )?;
    let mut fields = Map::new();
    fields.insert("side".to_owned(), json!(side));
    if start_side != side {
        fields.insert("start_side".to_owned(), json!(start_side));
    }
    fields.insert("start".to_owned(), json!({ start_side: start }));
    fields.insert("end".to_owned(), json!({ side: line }));
    fields.insert("anchor".to_owned(), anchor);
    Some(fields)
}

fn side_name(side: &Value) -> Option<&'static str> {
    match side.as_str()? {
        "LEFT" => Some("old"),
        "RIGHT" => Some("new"),
        _ => None,
    }
}

/// GitHub keeps a suggestion inside the body, as a closing fenced block.
fn split_suggestion(body: &str) -> (String, Option<String>) {
    let trimmed = body.trim_end();
    trimmed
        .strip_suffix("```")
        .and_then(|b| b.rsplit_once("```suggestion\n"))
        .filter(|(text, _)| text.is_empty() || text.ends_with('\n'))
        .map_or_else(
            || (body.to_owned(), None),
            |(text, suggestion)| {
                (
                    text.trim_end().to_owned(),
                    Some(
                        suggestion
                            .strip_suffix('\n')
                            .unwrap_or(suggestion)
                            .to_owned(),
                    ),
                )
            },
        )
}

/// A comment's body as GitHub keeps it.
pub(crate) fn body(comment: &Value) -> String {
    with_suggestion(
        comment["text"].as_str().unwrap_or_default(),
        comment["suggestion"].as_str(),
    )
}

fn with_suggestion(text: &str, suggestion: Option<&str>) -> String {
    let Some(suggestion) = suggestion else {
        return text.to_owned();
    };
    let block = if suggestion.is_empty() {
        "```suggestion\n```".to_owned()
    } else {
        format!("```suggestion\n{suggestion}\n```")
    };
    if text.is_empty() {
        block
    } else {
        format!("{text}\n\n{block}")
    }
}

/// A fence longer than any run of backticks in `text`.
fn fence(text: &str) -> String {
    let longest = text
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    "`".repeat(longest.max(2) + 1)
}

/// Whether GitHub takes `comment` on its lines: it has them, and its first
/// and last are in one of `hunks`, its file's in the PR's diff with
/// [`CONTEXT`] lines of context. Without them, by the comment's
/// `distance`, which was measured on the diff it was written on.
pub(crate) fn fits_its_lines(comment: &Value, hunks: Option<&[git::Hunk]>) -> bool {
    let side = comment["side"].as_str().unwrap_or("new");
    let start_side = comment["start_side"].as_str().unwrap_or(side);
    let (Some(start), Some(end)) = (
        comment["start"][start_side].as_u64(),
        comment["end"][side].as_u64(),
    ) else {
        return false;
    };
    match hunks {
        Some(hunks) => hunks
            .iter()
            .any(|h| h.side(start_side).contains(&start) && h.side(side).contains(&end)),
        None => comment["distance"].as_u64().is_none_or(|d| d <= CONTEXT),
    }
}

/// The draft's body, and the variables placing it, for a new thread: on
/// its lines, or `on_file`, a file comment that quotes the lines it has.
pub(crate) fn thread_variables(review: &str, comment: &Value, on_file: bool) -> Value {
    let text = comment["text"].as_str().unwrap_or_default();
    let suggestion = comment["suggestion"].as_str();
    let mut vars = json!({"review": review, "path": comment["path"]});
    let side = comment["side"].as_str().unwrap_or("new");
    let start_side = comment["start_side"].as_str().unwrap_or(side);
    let line = comment["end"][side].as_u64();
    let start = comment["start"][start_side].as_u64();
    match (line, start) {
        (Some(line), Some(start)) if !on_file => {
            vars["body"] = json!(with_suggestion(text, suggestion));
            vars["line"] = json!(line);
            vars["side"] = json!(diff_side(side));
            if start != line || start_side != side {
                vars["startLine"] = json!(start);
                vars["startSide"] = json!(diff_side(start_side));
            }
        }
        (Some(line), start) => {
            let quoted: Vec<&str> = comment["anchor"]["lines"]
                .as_array()
                .map(|l| l.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let quoted = quoted.join("\n");
            let lines = match start {
                Some(start) if start != line => format!("Lines {start}-{line}"),
                _ => format!("Line {line}"),
            };
            let on = if side == "old" {
                " on the old side"
            } else {
                ""
            };
            let suggested = suggestion
                .map(|s| {
                    let fence = fence(s);
                    format!("\n\nSuggested:\n{fence}\n{s}\n{fence}")
                })
                .unwrap_or_default();
            let fence = fence(&quoted);
            vars["body"] = json!(format!(
                "{lines}{on}:\n{fence}\n{quoted}\n{fence}\n\n{text}{suggested}"
            ));
            vars["subjectType"] = json!("FILE");
        }
        (None, _) => {
            vars["body"] = json!(with_suggestion(text, suggestion));
            vars["subjectType"] = json!("FILE");
        }
    }
    vars
}

fn diff_side(side: &str) -> &'static str {
    if side == "old" { "LEFT" } else { "RIGHT" }
}

/// What `post` needs before it sends anything.
pub(crate) struct PostState {
    pub pr_id: String,
    pub url: String,
    /// Your pending review.
    pub pending: Option<Review>,
}

/// A review: its id, and its url on GitHub.
pub(crate) type Review = (String, String);

pub(crate) fn post_state(remote: &Remote, pr: u64) -> Result<PostState, Error> {
    let data = graphql(
        "PostState",
        POST_STATE,
        &json!({"owner": remote.owner, "name": remote.name, "number": pr}),
    )?;
    let found = pull_request(&data, pr)?;
    let pending = &found["reviews"]["nodes"][0];
    Ok(PostState {
        pr_id: string(&found["id"]),
        url: string(&found["url"]),
        pending: pending["id"]
            .as_str()
            .map(|id| (id.to_owned(), string(&pending["url"]))),
    })
}

/// Starts a pending review on `commit`, the head the comments were made on.
pub(crate) fn add_review(pr_id: &str, commit: Option<&str>) -> Result<Review, Error> {
    let data = graphql(
        "AddReview",
        ADD_REVIEW,
        &json!({"pr": pr_id, "commit": commit}),
    )?;
    let review = &data["addPullRequestReview"]["pullRequestReview"];
    Ok((string(&review["id"]), string(&review["url"])))
}

/// A comment GitHub accepted.
pub(crate) struct Posted {
    pub id: String,
    pub url: Value,
}

fn posted(comment: &Value) -> Option<Posted> {
    Some(Posted {
        id: comment["id"].as_str()?.to_owned(),
        url: comment["url"].clone(),
    })
}

/// Posts a new thread; returns its first comment and the thread's id.
pub(crate) fn add_thread(variables: &Value) -> Result<(Posted, String), Error> {
    let data = graphql("AddPullRequestReviewThread", ADD_THREAD, variables)?;
    let thread = &data["addPullRequestReviewThread"]["thread"];
    posted(&thread["comments"]["nodes"][0])
        .zip(thread["id"].as_str().map(str::to_owned))
        .ok_or_else(|| Error::Rejected("GitHub returned no thread".to_owned()))
}

pub(crate) fn add_reply(review: &str, thread: &str, body: &str) -> Result<Posted, Error> {
    let data = graphql(
        "AddPullRequestReviewThreadReply",
        ADD_REPLY,
        &json!({"review": review, "thread": thread, "body": body}),
    )?;
    posted(&data["addPullRequestReviewThreadReply"]["comment"])
        .ok_or_else(|| Error::Rejected("GitHub returned no comment".to_owned()))
}

pub(crate) fn resolve(thread: &str, resolved: bool) -> Result<(), Error> {
    let (operation, query) = if resolved {
        ("ResolveReviewThread", RESOLVE)
    } else {
        ("UnresolveReviewThread", UNRESOLVE)
    };
    graphql(operation, query, &json!({"thread": thread})).map(drop)
}

pub(crate) fn submit(review: &str, event: &str, body: &str) -> Result<(), Error> {
    let mut vars = json!({"review": review, "event": event});
    if !body.is_empty() {
        vars["body"] = json!(body);
    }
    graphql("SubmitPullRequestReview", SUBMIT, &vars).map(drop)
}

pub(crate) fn set_review_body(review: &str, body: &str) -> Result<(), Error> {
    graphql(
        "UpdatePullRequestReview",
        UPDATE_REVIEW,
        &json!({"review": review, "body": body}),
    )
    .map(drop)
}

/// The open PR whose branch is `branch`, in origin's repository:
/// `{number, title, url, base, head}`.
pub(crate) fn detect(remote: &Remote, branch: &str) -> Result<Option<Value>, Error> {
    let data = graphql(
        "Detect",
        DETECT,
        &json!({"owner": remote.owner, "name": remote.name, "branch": branch}),
    )?;
    let pr = &data["repository"]["pullRequests"]["nodes"][0];
    Ok(pr["number"].as_u64().map(|number| {
        json!({"number": number, "title": pr["title"], "url": pr["url"],
            "base": pr["baseRefOid"], "head": pr["headRefOid"]})
    }))
}

/// The open PRs to pick from, each once: the current branch's, then those
/// asking for your review, then yours.
pub(crate) fn list(remote: &Remote, branch: Option<&str>) -> Result<Vec<Value>, Error> {
    let repo = format!("repo:{}/{}", remote.owner, remote.name);
    let data = graphql(
        "List",
        LIST,
        &json!({
            "owner": remote.owner,
            "name": remote.name,
            "requested": format!("is:pr is:open review-requested:@me {repo}"),
            "mine": format!("is:pr is:open author:@me {repo}"),
            "branch": branch.unwrap_or_default(),
            "onBranch": branch.is_some(),
        }),
    )?;
    let mut seen = HashSet::new();
    Ok([
        ("current_branch", &data["repository"]["pullRequests"]["nodes"]),
        ("review_requested", &data["requested"]["nodes"]),
        ("mine", &data["mine"]["nodes"]),
    ]
    .into_iter()
    .flat_map(|(reason, nodes)| {
        nodes
            .as_array()
            .into_iter()
            .flatten()
            .map(move |pr| (reason, pr))
    })
    // Search results that aren't PRs come back as empty objects.
    .filter(|(_, pr)| pr["number"].as_u64().is_some_and(|n| seen.insert(n)))
    .map(|(reason, pr)| {
        json!({
            "id": pr["number"].as_u64().unwrap_or_default().to_string(),
            "title": pr["title"],
            "author": pr["author"]["login"].as_str().unwrap_or("ghost"),
            "url": pr["url"],
            "head_ref": pr["headRefName"],
            "reason": reason,
        })
    })
    .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suggestion_round_trips_through_a_body() {
        for (text, suggestion) in [
            ("Spell it out.", Some("FIVE")),
            ("Drop these.", Some("")),
            ("", Some("a\nb")),
            ("No suggestion.\n\n```rust\nlet x;\n```", None),
        ] {
            let body = with_suggestion(text, suggestion);
            assert_eq!(
                split_suggestion(&body),
                (text.to_owned(), suggestion.map(str::to_owned)),
                "{body}"
            );
        }
    }

    #[test]
    fn a_line_comment_spans_sides_with_a_start_line() {
        let comment = json!({"path": "a.rs", "side": "new", "start_side": "old",
            "start": {"old": 3, "new": null}, "end": {"old": null, "new": 4},
            "text": "Hm.", "distance": 0});
        assert!(fits_its_lines(&comment, None));
        let hunk = |old, new| git::Hunk { old, new };
        assert!(fits_its_lines(&comment, Some(&[hunk(1..4, 2..6)])));
        assert!(!fits_its_lines(
            &comment,
            Some(&[hunk(1..4, 1..2), hunk(4..9, 4..9)])
        ));
        assert_eq!(
            thread_variables("R", &comment, false),
            json!({"review": "R", "path": "a.rs", "body": "Hm.", "line": 4, "side": "RIGHT",
                "startLine": 3, "startSide": "LEFT"})
        );
    }

    #[test]
    fn githubs_files_come_from_every_page_without_the_patches_it_left_out() {
        let pages =
            br#"[{"filename":"a.rs","patch":"@@ -1,2 +1,3 @@\n a\n+b\n c\n@@ -9 +10,0 @@\n-z"},
            {"filename":"big.rs","changes":90000},
            {"filename":"logo.png","changes":0}]
            [{"filename":"new.rs","previous_filename":"old.rs","changes":0}]"#;
        let files = parse_files(pages).expect("pages");
        let hunk = |old, new| git::Hunk { old, new };
        assert_eq!(files["a.rs"], [hunk(1..3, 1..4), hunk(9..10, 10..10)]);
        assert_eq!(files["logo.png"], []);
        assert_eq!(files["new.rs"], []);
        assert_eq!(files.len(), 3, "{:?}", files.keys());
        assert_eq!(parse_files(b"[{}] [oops"), None);
    }

    /// Not a test: compares GitHub's hunks of a PR with git's, file by
    /// file, in a clone that has the PR's base and head:
    /// `DFFY_CONFORMANCE='owner/repo#n /path/to/clone' cargo test -p
    /// dffy-review-github conformance -- --ignored --nocapture`.
    #[test]
    #[ignore = "asks GitHub, and needs a clone with the PR's commits"]
    fn conformance() {
        let spec = std::env::var("DFFY_CONFORMANCE")
            .expect("DFFY_CONFORMANCE='owner/repo#n /path/to/clone'");
        let (pr, clone) = spec.split_once(' ').expect("a PR and a clone");
        let (repo, number) = pr.split_once('#').expect("owner/repo#n");
        let (owner, name) = repo.split_once('/').expect("owner/repo");
        let remote = Remote {
            owner: owner.to_owned(),
            name: name.to_owned(),
            key: String::new(),
        };
        let number: u64 = number.parse().expect("a PR number");
        std::env::set_current_dir(clone).expect("the clone");
        let found = page(&remote, number, None).expect("the PR");
        for oid in [&found.base, &found.head] {
            assert!(
                git::has_commit(oid),
                "{oid} isn't in the clone; git fetch origin {oid}"
            );
        }
        let github = files(&remote, number).expect("the PR's files");
        let local = git::hunks(&found.base, &found.head, CONTEXT).expect("git's diff");

        let lines = |hunks: &[git::Hunk], side: &str| -> HashSet<u64> {
            hunks.iter().flat_map(|h| h.side(side).clone()).collect()
        };
        let (mut differ, mut only_github, mut only_git) = (Vec::new(), Vec::new(), Vec::new());
        let (mut hunks_github, mut hunks_git, mut lines_github, mut lines_git) = (0, 0, 0, 0);
        let mut paths: Vec<&String> = github.keys().chain(local.keys()).collect();
        paths.sort_unstable();
        paths.dedup();
        for path in paths {
            match (github.get(path), local.get(path)) {
                (Some(g), Some(l)) => {
                    let g_only = g.iter().filter(|h| !l.contains(h)).count();
                    let l_only = l.iter().filter(|h| !g.contains(h)).count();
                    if g_only + l_only > 0 {
                        differ.push(path);
                    }
                    hunks_github += g_only;
                    hunks_git += l_only;
                    for side in ["old", "new"] {
                        let (g, l) = (lines(g, side), lines(l, side));
                        lines_github += g.difference(&l).count();
                        lines_git += l.difference(&g).count();
                    }
                }
                // git leaves out a file with no hunks, renamed only or binary.
                (Some(g), None) if g.is_empty() => {}
                (Some(_), None) => only_github.push(path),
                (None, _) => only_git.push(path),
            }
        }
        println!(
            "{pr} at {}: {} files from GitHub with hunks or none, {} from git",
            found.head,
            github.len(),
            local.len()
        );
        println!(
            "files: {} with different hunks, {} only in GitHub's, {} only in git's \
             (GitHub left the patch out, or past its 3000)",
            differ.len(),
            only_github.len(),
            only_git.len()
        );
        println!(
            "hunks: {hunks_github} of GitHub's not in git's, {hunks_git} of git's not in GitHub's"
        );
        println!(
            "lines commentable: {lines_github} only by GitHub's hunks, {lines_git} only by git's"
        );
        for path in differ {
            println!("  differs: {path}");
        }
        for path in only_github {
            println!("  only GitHub's: {path}");
        }
        for path in only_git {
            println!("  only git's: {path}");
        }
    }

    #[test]
    fn a_quoted_line_with_backticks_gets_a_longer_fence() {
        let comment = json!({"path": "a.md", "side": "old",
            "start": {"old": 8, "new": 8}, "end": {"old": 8, "new": 8},
            "anchor": {"lines": ["```sh"]}, "text": "Why?", "distance": 9});
        assert!(!fits_its_lines(&comment, None));
        assert_eq!(
            thread_variables("R", &comment, true)["body"],
            "Line 8 on the old side:\n````\n```sh\n````\n\nWhy?"
        );
    }
}

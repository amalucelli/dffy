//! GitHub through `gh api graphql`, which owns auth, tokens and hosts: the
//! queries and mutations, and the mapping between GitHub's review threads
//! and the comments file.

use std::collections::HashSet;
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
          id isResolved isOutdated path diffSide startDiffSide line startLine originalLine originalStartLine subjectType
          comments(first: 100) {
            nodes { id url body createdAt viewerDidAuthor author { login } commit { oid } originalCommit { oid } }
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

#[derive(Debug)]
pub(crate) enum Error {
    /// GitHub answered and refused this request; others may still work.
    Rejected(String),
    /// No answer, or a rate limit: stop.
    Failed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(m) | Self::Failed(m) => f.write_str(m),
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
        .map_err(|e| Error::Failed(format!("can't run gh: {e}")))?;
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

/// The comments file's operations for a page of threads: each thread's
/// first comment, with its lines and an anchor read from git, then its
/// replies. A thread whose lines git can't read comes in as a file comment.
/// `toggled` holds the refs of threads resolved or reopened here and not
/// posted yet, whose `resolved` stays as it is.
pub(crate) fn upserts(
    threads: &[Value],
    base: &str,
    source: &mut Source,
    toggled: &HashSet<String>,
) -> Vec<Value> {
    let mut ops = Vec::new();
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
        if !toggled.contains(&root_ref) {
            comment.insert("resolved".to_owned(), thread["isResolved"].clone());
        }
        // Only when true: an upsert that adds a comment keeps a null field.
        if thread["isOutdated"] == true {
            comment.insert("outdated".to_owned(), json!(true));
        }
        if let Some(lines) = placed(thread, root, base, source) {
            comment.extend(lines);
        }
        ops.push(json!({"op": "upsert", "comment": comment}));
        for reply in replies {
            let mut comment = imported(reply, &format!("github:{}", string(&reply["id"])));
            comment.insert("in_reply_to_ref".to_owned(), json!(root_ref));
            ops.push(json!({"op": "upsert", "comment": comment}));
        }
    }
    ops
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

/// A line thread's side, lines and anchor. GitHub gives the current lines
/// against the comment's `commit` and, once they're outdated, the original
/// ones against its `originalCommit`; an old-side line is in that commit's
/// merge base with the PR's base.
fn placed(
    thread: &Value,
    root: &Value,
    base: &str,
    source: &mut Source,
) -> Option<Map<String, Value>> {
    if thread["subjectType"] != "LINE" {
        return None;
    }
    let (line, start, commit) = match thread["line"].as_u64() {
        Some(line) => (line, &thread["startLine"], &root["commit"]["oid"]),
        None => (
            thread["originalLine"].as_u64()?,
            &thread["originalStartLine"],
            &root["originalCommit"]["oid"],
        ),
    };
    let commit = commit.as_str()?;
    let side = side_name(&thread["diffSide"])?;
    let start_side = side_name(&thread["startDiffSide"]).unwrap_or(side);
    let start = start.as_u64().unwrap_or(line);
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

/// Whether GitHub takes `comment` on its lines: it has them, and they're
/// no further than [`CONTEXT`] from a change.
pub(crate) fn fits_its_lines(comment: &Value) -> bool {
    let side = comment["side"].as_str().unwrap_or("new");
    let start_side = comment["start_side"].as_str().unwrap_or(side);
    comment["end"][side].is_u64()
        && comment["start"][start_side].is_u64()
        && comment["distance"].as_u64().is_none_or(|d| d <= CONTEXT)
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
            "number": pr["number"],
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
        assert!(fits_its_lines(&comment));
        assert_eq!(
            thread_variables("R", &comment, false),
            json!({"review": "R", "path": "a.rs", "body": "Hm.", "line": 4, "side": "RIGHT",
                "startLine": 3, "startSide": "LEFT"})
        );
    }

    #[test]
    fn a_quoted_line_with_backticks_gets_a_longer_fence() {
        let comment = json!({"path": "a.md", "side": "old",
            "start": {"old": 8, "new": 8}, "end": {"old": 8, "new": 8},
            "anchor": {"lines": ["```sh"]}, "text": "Why?", "distance": 9});
        assert!(!fits_its_lines(&comment));
        assert_eq!(
            thread_variables("R", &comment, true)["body"],
            "Line 8 on the old side:\n````\n```sh\n````\n\nWhy?"
        );
    }
}

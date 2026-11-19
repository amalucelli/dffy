//! GitHub through `gh api`, which owns auth, tokens and hosts: the GraphQL
//! queries and mutations, the mapping between GitHub's review threads and
//! the comments file, and the PR's diff as GitHub has it.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use serde_json::{Map, Value, json};

use crate::git::{self, Head, Remote, Source};

/// Threads a page; at 100 comments each it stays under GitHub's node limit.
const THREADS_PER_PAGE: u32 = 100;

/// Hosts accept a line comment only this close to a change.
pub(crate) const CONTEXT: u64 = 3;

/// What an import reads of a comment, on a thread's first page of them and
/// on the pages after.
macro_rules! review_comment {
    () => {
        "fragment ReviewComment on PullRequestReviewComment {
  id url body createdAt state viewerDidAuthor author { login } originalCommit { oid }
}"
    };
}

/// `reviews` names your pending review's comments, so an import never
/// takes them for deleted, whether or not `reviewThreads` lists them.
/// The PR's body, its `conversation` and its `submitted` reviews are its
/// general comments; a page after the first asks only for the lists with
/// more to read. A pending review isn't among them: it's your summary not
/// sent yet. `latest`, on the first page, finds the commit of your last
/// submitted review among the last 20.
const PULL_REQUEST: &str = concat!(
    "query PullRequest($owner: String!, $name: String!, $number: Int!, $after: String, $first: Int!, $threads: Boolean!, $comments: Boolean!, $commentsAfter: String, $reviews: Boolean!, $reviewsAfter: String, $latest: Boolean!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      id title url body createdAt updatedAt viewerDidAuthor author { login }
      baseRefOid headRefOid headRefName isCrossRepository headRepositoryOwner { login } state
      reviews(states: [PENDING], first: 1) {
        nodes { comments(first: 100) { pageInfo { hasNextPage endCursor } nodes { id } } }
      }
      latest: reviews(last: 20, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) @include(if: $latest) {
        nodes { viewerDidAuthor commit { oid } }
      }
      conversation: comments(first: 100, after: $commentsAfter) @include(if: $comments) {
        pageInfo { hasNextPage endCursor }
        nodes { id url body createdAt viewerDidAuthor author { login } }
      }
      submitted: reviews(first: 100, after: $reviewsAfter, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) @include(if: $reviews) {
        pageInfo { hasNextPage endCursor }
        nodes { id url body state submittedAt createdAt viewerDidAuthor author { login } }
      }
      reviewThreads(first: $first, after: $after) @include(if: $threads) {
        totalCount
        pageInfo { hasNextPage endCursor }
        nodes {
          id isResolved isOutdated path diffSide startDiffSide originalLine originalStartLine subjectType
          comments(first: 100) {
            pageInfo { hasNextPage endCursor }
            nodes { ...ReviewComment }
          }
        }
      }
    }
  }
}
",
    review_comment!()
);

const THREAD_COMMENTS: &str = concat!(
    "query ThreadComments($thread: ID!, $after: String!) {
  node(id: $thread) {
    ... on PullRequestReviewThread {
      comments(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes { ...ReviewComment }
      }
    }
  }
}
",
    review_comment!()
);

/// Your pending review's comments past the first page of them.
const PENDING_COMMENTS: &str = "query PendingComments($owner: String!, $name: String!, $number: Int!, $after: String!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviews(states: [PENDING], first: 1) {
        nodes { comments(first: 100, after: $after) { pageInfo { hasNextPage endCursor } nodes { id } } }
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
      reviews(states: [PENDING], first: 1) { nodes { id url comments { totalCount } } }
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
/// than asking for every PR. The current branch's are found as `DETECT`
/// finds them.
const LIST: &str = "query List($owner: String!, $name: String!, $requested: String!, $mine: String!, $branch: String!, $onBranch: Boolean!, $number: Int!, $byNumber: Boolean!) {
  requested: search(query: $requested, type: ISSUE, first: 50) { nodes { ...Listed } }
  mine: search(query: $mine, type: ISSUE, first: 50) { nodes { ...Listed } }
  repository(owner: $owner, name: $name) @include(if: $onBranch) {
    pullRequests(headRefName: $branch, states: OPEN, first: 50) @skip(if: $byNumber) { nodes { ...Listed } }
    pullRequest(number: $number) @include(if: $byNumber) { ...Listed }
  }
}
fragment Listed on PullRequest { number title url headRefName author { login } isCrossRepository headRepositoryOwner { login } state }";

/// By the branch's name, or by number for a PR `gh pr checkout` had the
/// branch track by its own ref. Forks' PRs from branches of the same name
/// come too, so a fork's PR past the first 50 of them isn't found.
const DETECT: &str = "query Detect($owner: String!, $name: String!, $branch: String!, $number: Int!, $byNumber: Boolean!) {
  repository(owner: $owner, name: $name) {
    pullRequests(headRefName: $branch, states: OPEN, first: 50) @skip(if: $byNumber) { nodes { ...Detected } }
    pullRequest(number: $number) @include(if: $byNumber) { ...Detected }
  }
}
fragment Detected on PullRequest { number title url baseRefOid headRefOid isCrossRepository headRepositoryOwner { login } state }";

pub(crate) const NO_GH: &str =
    "dffy-review-github needs the GitHub CLI (gh): https://cli.github.com";

/// gh's own host when it's given none: `GH_HOST`, else github.com.
pub(crate) fn default_host() -> String {
    std::env::var("GH_HOST")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "github.com".to_owned())
}

static HOST: OnceLock<String> = OnceLock::new();

/// Sends every API call of the run to `host`, the repository's, as
/// `--hostname`. A run sets it once, before its first call.
pub(crate) fn use_host(host: &str) {
    HOST.get_or_init(|| host.to_owned());
}

fn host() -> String {
    HOST.get().cloned().unwrap_or_else(default_host)
}

pub(crate) fn logged_out() -> String {
    format!("gh isn't logged in to {}; run `gh auth login`", host())
}

/// Whether gh has a login for `host`, by `gh auth status --hostname`,
/// which lists the host only then; it asks nothing of a host it has no
/// login for. `None` when gh can't run.
pub(crate) fn logged_in(host: &str) -> Option<bool> {
    let out = Command::new("gh")
        .args(["auth", "status", "--hostname", host])
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    // Older gh writes it to stderr.
    let said = [out.stdout, out.stderr].concat();
    Some(
        String::from_utf8_lossy(&said)
            .lines()
            .any(|l| l.trim_end() == host),
    )
}

/// "resets at 14:05 UTC", for a rate limit reset at `epoch` seconds.
fn resets(epoch: u64) -> String {
    let (hours, minutes) = (epoch % 86_400 / 3600, epoch % 3600 / 60);
    format!("resets at {hours:02}:{minutes:02} UTC")
}

/// When the GraphQL rate limit resets, from the REST endpoint that says,
/// which costs no rate limit.
fn graphql_reset() -> Option<String> {
    let out = Command::new("gh")
        .args(["api", "--hostname", &host(), "rate_limit"])
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let answer: Value = serde_json::from_slice(&out.stdout).ok()?;
    answer["resources"]["graphql"]["reset"].as_u64().map(resets)
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
        .args(["api", "--hostname", &host(), "graphql", "--input", "-"])
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
        let primary = errors.iter().any(|e| e["type"] == "RATE_LIMITED");
        let limited = primary || message.to_lowercase().contains("rate limit");
        return Err(if limited {
            // A secondary limit's reset isn't the one `rate_limit` names.
            match graphql_reset().filter(|_| primary) {
                Some(reset) => Error::Failed(format!("{message}; {reset}")),
                None => Error::Failed(message),
            }
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
    /// What the session keeps of the PR: `title`, `url`, its branch in
    /// `head_ref`, `state`, whether it's a fork's in `cross_repository`,
    /// the branch's owner in `head_owner`, and when it last changed in
    /// `updated_at`.
    pub about: Value,
    pub base: String,
    pub head: String,
    /// Each thread's comments are only its first page of them until
    /// [`rest_of_thread`] adds the others.
    pub threads: Vec<Value>,
    /// The PR's threads, on every page.
    pub total: u64,
    /// Where the next page starts; `None` after the last.
    pub next: Option<After>,
    /// The ids of your pending review's first page of comments, and where
    /// the next page of them starts, for [`rest_of_pending`].
    pub pending: (Vec<String>, Option<String>),
    /// The commit of your last submitted review, on the first page.
    pub reviewed: Option<String>,
    /// The page's general comments, as the comments file holds them, by
    /// when they were written: the PR's description on the first page,
    /// then comments in its conversation and the summaries of reviews.
    pub conversation: Vec<Value>,
}

/// Where each of a PR's lists goes on, for the lists with more to read.
#[derive(Debug, Default, Clone)]
pub(crate) struct After {
    pub threads: Option<String>,
    pub comments: Option<String>,
    pub reviews: Option<String>,
}

/// `list`'s cursor, when it has more.
fn more(list: &Value) -> Option<String> {
    list["pageInfo"]["endCursor"]
        .as_str()
        .filter(|_| list["pageInfo"]["hasNextPage"] == true)
        .map(str::to_owned)
}

/// The first page with `None`, which asks for every list from its start.
pub(crate) fn page(remote: &Remote, pr: u64, after: Option<&After>) -> Result<Page, Error> {
    let mut vars = json!({"owner": remote.owner, "name": remote.name, "number": pr, "first": THREADS_PER_PAGE});
    let lists = [
        ("after", "threads", after.map(|a| &a.threads)),
        ("commentsAfter", "comments", after.map(|a| &a.comments)),
        ("reviewsAfter", "reviews", after.map(|a| &a.reviews)),
    ];
    for (cursor, wanted, from) in lists {
        vars[cursor] = json!(from.cloned().flatten());
        vars[wanted] = json!(from.is_none_or(Option::is_some));
    }
    vars["latest"] = json!(after.is_none());
    let mut data = graphql("PullRequest", PULL_REQUEST, &vars)?;
    let nodes = data
        .pointer_mut("/repository/pullRequest/reviewThreads/nodes")
        .map(Value::take);
    let found = pull_request(&data, pr)?;
    let threads = &found["reviewThreads"];
    let reviewed = found["latest"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .rfind(|r| r["viewerDidAuthor"] == true)
        .and_then(|r| r["commit"]["oid"].as_str())
        .map(str::to_owned);
    Ok(Page {
        about: json!({"title": found["title"], "url": found["url"], "head_ref": found["headRefName"],
            "state": found["state"], "cross_repository": found["isCrossRepository"],
            "head_owner": found["headRepositoryOwner"]["login"], "updated_at": found["updatedAt"]}),
        base: string(&found["baseRefOid"]),
        head: string(&found["headRefOid"]),
        pending: pending_ids(&found["reviews"]["nodes"][0]["comments"]),
        reviewed,
        threads: match nodes {
            Some(Value::Array(nodes)) => nodes,
            _ => Vec::new(),
        },
        total: threads["totalCount"].as_u64().unwrap_or_default(),
        next: Some(After {
            threads: more(threads),
            comments: more(&found["conversation"]),
            reviews: more(&found["submitted"]),
        })
        .filter(|a| a.threads.is_some() || a.comments.is_some() || a.reviews.is_some()),
        conversation: conversation(found, after.is_none()),
    })
}

/// A page of your pending review's comments: their ids, and where the next
/// page starts.
fn pending_ids(comments: &Value) -> (Vec<String>, Option<String>) {
    let ids = comments["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["id"].as_str().map(str::to_owned))
        .collect();
    (ids, more(comments))
}

/// The ids of your pending review's comments from `after` on, a query a
/// page.
pub(crate) fn rest_of_pending(
    remote: &Remote,
    pr: u64,
    mut after: String,
) -> Result<Vec<String>, Error> {
    let mut all = Vec::new();
    loop {
        let data = graphql(
            "PendingComments",
            PENDING_COMMENTS,
            &json!({"owner": remote.owner, "name": remote.name, "number": pr, "after": after}),
        )?;
        let (ids, next) = pending_ids(&pull_request(&data, pr)?["reviews"]["nodes"][0]["comments"]);
        all.extend(ids);
        match next {
            Some(next) => after = next,
            None => return Ok(all),
        }
    }
}

/// The general comments of `pr`, a page of the PR: its description when
/// `first`, the comments in its conversation and the summaries of its
/// reviews, by when each was written. Those without text are left out, as
/// a review submitted with comments on lines only.
fn conversation(pr: &Value, first: bool) -> Vec<Value> {
    let nodes = |list: &str| pr[list]["nodes"].as_array().into_iter().flatten();
    let description = first.then_some((pr, "description", None));
    let comments = nodes("conversation").map(|c| (c, "comment", None));
    let reviews = nodes("submitted").map(|r| {
        let label = match r["state"].as_str() {
            Some("APPROVED") => Some("approved"),
            Some("CHANGES_REQUESTED") => Some("changes requested"),
            Some("COMMENTED") => Some("commented"),
            Some("DISMISSED") => Some("dismissed"),
            _ => None,
        };
        (r, "review", label)
    });
    let mut out: Vec<Value> = description
        .into_iter()
        .chain(comments)
        .chain(reviews)
        .filter_map(|(node, kind, label)| {
            let text = string(&node["body"]).replace("\r\n", "\n");
            if text.trim().is_empty() {
                return None;
            }
            let author = if node["viewerDidAuthor"] == true {
                ""
            } else {
                node["author"]["login"].as_str().unwrap_or("ghost")
            };
            // A review joins the conversation when it's submitted.
            let created = if node["submittedAt"].is_string() {
                &node["submittedAt"]
            } else {
                &node["createdAt"]
            };
            let mut comment = json!({"ref": format!("github:{}", string(&node["id"])),
                "url": node["url"], "text": text, "author": author, "created": created,
                "kind": kind});
            if let Some(label) = label {
                comment["label"] = json!(label);
            }
            Some(comment)
        })
        .collect();
    out.sort_by(|a, b| string(&a["created"]).cmp(&string(&b["created"])));
    out
}

/// Adds to a thread from a [`Page`] the comments past its first page, a
/// query a page.
pub(crate) fn rest_of_thread(thread: &mut Value) -> Result<(), Error> {
    loop {
        let info = &thread["comments"]["pageInfo"];
        let Some(after) = info["endCursor"]
            .as_str()
            .filter(|_| info["hasNextPage"] == true)
            .map(str::to_owned)
        else {
            return Ok(());
        };
        let mut data = graphql(
            "ThreadComments",
            THREAD_COMMENTS,
            &json!({"thread": thread["id"], "after": after}),
        )?;
        let mut more = data["node"]["comments"].take();
        if let (Some(have), Some(nodes)) = (
            thread["comments"]["nodes"].as_array_mut(),
            more["nodes"].as_array_mut(),
        ) {
            have.append(nodes);
        }
        thread["comments"]["pageInfo"] = more["pageInfo"].take();
    }
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
            "--hostname",
            &host(),
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
    /// With `pending`, in your pending review, which goes once it's sent.
    pub pending: HashSet<String>,
}

impl Known {
    /// `pending` for the comment `node` of `reference`, if it changed: set
    /// while it's in your pending review, cleared once it isn't.
    fn pending(&mut self, node: &Value, reference: &str, comment: &mut Map<String, Value>) {
        if node["state"] == "PENDING" {
            self.pending.insert(reference.to_owned());
            comment.insert("pending".to_owned(), json!(true));
        } else if self.pending.remove(reference) {
            comment.insert("pending".to_owned(), Value::Null);
        }
    }
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
        known.pending(root, &root_ref, &mut comment);
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
            comment.insert(
                "github_unplaced".to_owned(),
                json!("original commit not available"),
            );
            known.unplaced.insert(root_ref.clone());
            waiting.push(thread.clone());
        } else if known.unplaced.remove(&root_ref) {
            comment.insert("github_unplaced".to_owned(), Value::Null);
        }
        ops.push(json!({"op": "upsert", "comment": comment}));
        for reply in replies {
            let reference = format!("github:{}", string(&reply["id"]));
            let mut comment = imported(reply, &reference);
            known.pending(reply, &reference, &mut comment);
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
    body.trim_end()
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
    /// The comments in it.
    pub pending_comments: u64,
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
        pending_comments: pending["comments"]["totalCount"]
            .as_u64()
            .unwrap_or_default(),
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

/// `DETECT`'s and `LIST`'s variables for the PRs `head` is the branch of.
fn head_variables(head: Option<&Head>) -> Value {
    let (branch, number) = match head {
        Some(Head::Pull(pr)) => ("", Some(*pr)),
        Some(Head::Branch { name, .. }) => (name.as_str(), None),
        None => ("", None),
    };
    json!({"branch": branch, "number": number.unwrap_or_default(), "byNumber": number.is_some()})
}

/// The open PRs `head` is the branch of in the repository `DETECT` or
/// `LIST` answered with.
fn heads<'a>(repository: &'a Value, head: &Head) -> Vec<&'a Value> {
    match head {
        Head::Pull(_) => Some(&repository["pullRequest"])
            .filter(|pr| pr["state"] == "OPEN")
            .into_iter()
            .collect(),
        Head::Branch { .. } => repository["pullRequests"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|pr| {
                head.owns(
                    &pr["isCrossRepository"],
                    &pr["headRepositoryOwner"]["login"],
                )
            })
            .collect(),
    }
}

/// The open PR `head` is the branch of: `{number, title, url, base, head}`.
pub(crate) fn detect(remote: &Remote, head: &Head) -> Result<Option<Value>, Error> {
    let mut vars = head_variables(Some(head));
    vars["owner"] = json!(remote.owner);
    vars["name"] = json!(remote.name);
    let data = graphql("Detect", DETECT, &vars)?;
    let Some(pr) = heads(&data["repository"], head).into_iter().next() else {
        return Ok(None);
    };
    Ok(pr["number"].as_u64().map(|number| {
        json!({"number": number, "title": pr["title"], "url": pr["url"],
            "base": pr["baseRefOid"], "head": pr["headRefOid"], "state": pr["state"]})
    }))
}

/// What `check` keeps of the PR between runs: the `ETag` of GitHub's last
/// answer, its head and when it last changed.
pub(crate) struct Checked {
    pub etag: Option<String>,
    pub head: String,
    pub updated_at: String,
}

/// The PR as GitHub has it now, by a conditional request: `None` when it
/// hasn't changed since the answer `etag` is of, which GitHub doesn't
/// count against the rate limit.
pub(crate) fn pull(remote: &Remote, pr: u64, etag: Option<&str>) -> Result<Option<Checked>, Error> {
    let path = format!("repos/{}/{}/pulls/{pr}", remote.owner, remote.name);
    let host = host();
    let mut args = vec!["api", "--hostname", &host, &path, "--include"];
    let condition = etag.map(|e| format!("If-None-Match: {e}"));
    if let Some(condition) = &condition {
        args.extend(["--header", condition]);
    }
    let out = Command::new("gh")
        .args(&args)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::Setup(NO_GH.to_owned()),
            _ => Error::Failed(format!("can't run gh: {e}")),
        })?;
    // `--include` prints the status line and headers, then the body; gh
    // exits 1 past a 2xx, 304 included.
    let said = String::from_utf8_lossy(&out.stdout);
    let (head, body) = said
        .split_once("\r\n\r\n")
        .or_else(|| said.split_once("\n\n"))
        .unwrap_or((&said, ""));
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1)?.parse().ok())
        .unwrap_or_default();
    let header = |name: &str| {
        head.lines().find_map(|l| {
            let (key, value) = l.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    };
    match status {
        304 => return Ok(None),
        200..=299 => {}
        401 => return Err(Error::Setup(logged_out())),
        403 | 429 if header("X-RateLimit-Remaining").as_deref() == Some("0") => {
            let reset = header("X-RateLimit-Reset")
                .and_then(|r| r.parse().ok())
                .map(|r| format!("; {}", resets(r)))
                .unwrap_or_default();
            return Err(Error::Failed(format!(
                "GitHub's rate limit is used up{reset}"
            )));
        }
        _ => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(Error::Failed(match stderr.trim() {
                "" => format!("gh exited with {}", out.status),
                said => said.to_owned(),
            }));
        }
    }
    let found: Value = serde_json::from_str(body)
        .map_err(|e| Error::Failed(format!("GitHub's answer for #{pr}: {e}")))?;
    Ok(Some(Checked {
        etag: header("ETag"),
        head: string(&found["head"]["sha"]),
        updated_at: string(&found["updated_at"]),
    }))
}

/// The open PRs to pick from, each once: those the current branch is the
/// head of, then those asking for your review, then yours.
pub(crate) fn list(remote: &Remote, head: Option<&Head>) -> Result<Vec<Value>, Error> {
    let repo = format!("repo:{}/{}", remote.owner, remote.name);
    let mut vars = head_variables(head);
    for (key, value) in [
        ("owner", json!(remote.owner)),
        ("name", json!(remote.name)),
        (
            "requested",
            json!(format!("is:pr is:open review-requested:@me {repo}")),
        ),
        ("mine", json!(format!("is:pr is:open author:@me {repo}"))),
        ("onBranch", json!(head.is_some())),
    ] {
        vars[key] = value;
    }
    let data = graphql("List", LIST, &vars)?;
    let current = head
        .map(|h| heads(&data["repository"], h))
        .unwrap_or_default();
    let mut seen = HashSet::new();
    Ok(current
        .into_iter()
        .map(|pr| ("current_branch", pr))
        .chain(
            [
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
            }),
        )
    // Search results that aren't PRs come back as empty objects.
    .filter_map(|(reason, pr)| {
        let number = pr["number"].as_u64().filter(|n| seen.insert(*n))?;
        Some(json!({
            "id": number.to_string(),
            "title": pr["title"],
            "author": pr["author"]["login"].as_str().unwrap_or("ghost"),
            "url": pr["url"],
            "head_ref": pr["headRefName"],
            "reason": reason,
        }))
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
            git_name: "origin".to_owned(),
            host: "github.com".to_owned(),
            owner: owner.to_owned(),
            name: name.to_owned(),
            key: String::new(),
        };
        let number: u64 = number.parse().expect("a PR number");
        std::env::set_current_dir(clone).expect("the clone");
        let found = page(&remote, number, None).expect("the PR");
        let mut source = Source::default();
        for oid in [&found.base, &found.head] {
            assert!(
                source.has_commit(oid),
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

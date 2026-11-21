# dffy-review-github

**Experimental.** It may change with the [review protocol](plugins.md#the-review-protocol-version-1).

`dffy-review-github` is the review plugin for GitHub pull requests. It reviews a PR in dffy: dffy opens and draws it, the plugin talks to GitHub and git. [plugins.md](plugins.md#review-plugins) covers enabling it and the protocol it speaks; this page is what it does on its side. Run it in a clone with a remote on GitHub.

## The repository and the host

- **The repository.** It takes the remote `gh` takes: the one `gh repo set-default` marked (`remote.<name>.gh-resolved`), else `upstream` when it's on a GitHub host, as in a fork's clone, else `origin`. URLs are read as git rewrites them (`url.<base>.insteadOf`). That remote is the one fetched from, and its URL names the host, owner and repository.
- **Hosts.** A host is GitHub's when it's github.com, `GH_HOST`, or one `gh auth status --hostname <host>` lists, which gh answers without asking a host it has no login for. The answer is kept beside the repository's PRs (`host.json`) for a day, or until gh's program or its `hosts.yml` changes. Every `gh api` call names the host with `--hostname`. In a repository without such a remote, `detect` answers `{"review":null}` without asking anything of the network, and the other requests fail saying which remote and host they found.
- **`gh`.** It reaches GitHub only through the [GitHub CLI](https://cli.github.com), logged in to the repository's host, which its `doctor` checks (gh's own host, `GH_HOST` or github.com, outside a repository). `doctor` runs `gh --version` and `gh auth status` side by side, each for 2 seconds at most: one that takes longer is killed, and its row fails saying so (`` `gh auth status` didn't answer in 2s ``). Without `gh` on `PATH`, a request that asks GitHub fails with `dffy-review-github needs the GitHub CLI (gh): https://cli.github.com`; logged out, or with a token GitHub refuses, with ``gh isn't logged in to github.com; run `gh auth login` ``. Neither costs a call of its own: the first request tells. A `prepare` that has to ask GitHub then sends no session; one answered from its session still opens, with the error after it.
- **Rate limits.** When GitHub's rate limit is used up, the error says when it resets (`resets at 14:05 UTC`), read from the answer's `X-RateLimit-Reset`, or for GraphQL from the `rate_limit` endpoint, which costs no rate limit. A secondary limit's message is GitHub's alone.

## Storage

The PR's comments are dffy's ([comments.md](comments.md#where-the-file-is)). The plugin keeps, under `$XDG_DATA_HOME/dffy-review-github/<repository>/<pr>`:

- the PR's session;
- GitHub's hunks of the PR at the session's head;
- its watcher's last answer from GitHub (`check.json`).

Beside the PRs' directories, per repository: `host.json`, `detect.json` (the branch `detect` last found no PR for) and `pruned` (when `prune` last ran).

A `comments.json` in a PR's directory is the PR's comments file from before dffy kept them. The plugin no longer reads it. Bring it into dffy's with `dffy comments --session import <that file>` once the PR is open ([comments.md](comments.md#importing-dffy-comments-import)), then remove it: while it holds a comment without a `ref`, or can't be read, `prune` keeps the PR's directory.

## Opening

The code comes only through `git fetch`, as the ref `refs/dffy/pull/<pr>`.

- A PR prepared before, whose commits are still in the clone, answers from its session before asking GitHub anything. Its threads are downloaded after that. When GitHub's first page has another head than the session's, it pushes `head/moved` before the progress.
- The session's head stays the one dffy shows until a `pull`, even when the PR has moved on. The watcher says when it has.
- When GitHub can't be reached, the code still opens on the fetched head, with the base the session had or the default branch's tip, and the threads are left as they were.
- The session's `checkout` is `gh pr checkout <pr>`, which dffy shows where it needs the PR checked out, as to apply a suggestion.

## Threads

Threads are merged into the file by `ref`, and drafts are never touched. A thread you resolved or reopened in dffy keeps your state until it's posted. Each comment gets its `url`, and a thread GitHub marks outdated gets `outdated`. A thread's lines and anchor are those it was written on, read from its original commit (at that commit's merge base with the base, for the old side), outdated or not: GitHub's own placement on the current head is ignored, and dffy places the thread by its anchor's text.

- An original commit a force-push left on no ref isn't brought by the head's fetch. After the last page, the original commits the clone doesn't have are fetched by oid in one `git fetch`, onto no ref, and their threads are placed then. Until then, and for good when GitHub no longer has the commit, the thread is a file comment with `"github_unplaced":"original commit not available"`. A commit GitHub lacks fails the whole fetch, so it's fetched again without the one git names.
- Once placed, a thread keeps its lines even when its commit later goes from the clone, so the commit is never fetched again for it. One GitHub has dropped is asked for again on each `prepare` or `pull`, after the session line.
- A comment in your pending review on GitHub, posted with the `pending` event or on GitHub's site, comes in with `"pending":true`, which goes once the review is submitted.
- A comment GitHub no longer has goes from the file once every page is read, by dffy's rules for [`gone`](comments.md#gone): a thread holding your drafts stays. Your pending review's comments count as there, every page of them.

## Conversation

The PR's [general comments](comments.md#general-comments) come with its threads, in the same requests, a page of each list at a time: a later page asks only for the lists that have more. They're merged by `ref` too, each with its `url`, and the viewer shows them as the review's [Conversation](viewer.md#conversation).

- The PR's description, `"kind":"description"`, written by the PR's author when the PR was opened. An empty description is left out.
- Each comment in the PR's conversation, `"kind":"comment"`.
- The summary of each review submitted, `"kind":"review"`, dated when it was submitted, with its state as its `label`: `approved`, `changes requested`, `commented` or `dismissed`. A review submitted without a summary, as one with comments on lines only, is left out, and so is your pending review: its summary isn't sent yet.
- Progress lines count threads only.
- One GitHub no longer has, or whose text was emptied, goes from the file once every page is read, as deleted threads do; one you replied to in a draft stays, without its `ref`, and the draft isn't posted.

## Posting

- Each comment is its own request, paced to stay under GitHub's limit, and its `ref` is sent for dffy to write as soon as it's posted, so posting again never sends one twice, even after the plugin crashed partway.
- A post runs alone: it waits for a `prepare` or `pull` still running, which dffy cancelled, and none starts until it's done. Dry runs, `list` and `detect` run beside anything.
- Comments are posted against the session's head, even when the PR has moved on.
- Before sending, each draft is planned from git and the hunks `prepare` or `pull` kept, without asking GitHub; `dryRun` says the same plan without sending anything:
  - A draft goes on the lines its anchor has in the file at that head (at its merge base with the base, for the old side), not on the line numbers it was written at: a draft written on the working tree, with lines added above it, moves to where its lines are. One whose lines aren't at the head is skipped: `lines not in the PR head (unpushed changes?)`.
  - A draft on a file the PR doesn't change is skipped: `file not in the PR`.
  - A line comment goes on its lines only when its first and last line, where they are at the head, are in one hunk of GitHub's diff of the PR; else it becomes a file comment that quotes the lines, since GitHub refuses it on them. `prepare` and `pull` ask GitHub for the PR's files beside its threads and keep their hunks for the head the session is on.
  - Where GitHub's hunks aren't kept for the session's head, or GitHub left a file's patch out (too large, or past the 3000 files it lists), git's diff of the PR (`base...head`, 3 lines of context, two hunks one line apart joined into one as GitHub joins them) decides instead, and the draft's plan says `"unsure":true`: git can still draw a hunk's edges where GitHub doesn't. The draft's `distance` was measured on the diff it was written on, so it's used only when git can't diff the PR either.
  - A reply goes in its thread when the thread is on GitHub, or its first comment is planned to post before it; else it's skipped: `its thread isn't posted`.
- Only your drafts are posted: a comment without a `ref` whose `author` isn't empty, such as an agent's, is left alone, neither planned nor counted.
- Each comment posted goes in your pending review, marked `"pending":true` until the review is submitted.
- A skipped draft stays a draft, its reason in the answer's `unposted`, and the review is still submitted with the event: dffy showed the skips before you sent.
- A draft GitHub refuses while posting stays a draft too, with GitHub's reason in its `post_error`, which goes once it's posted. The review is still submitted with the event, without it.
- Only GitHub no longer answering stops a post: the event isn't applied, the error says so, and the next post sends the rest. The file's `notice` holds only what's about the whole run: a stop, a failed submit, resolves GitHub refused.
- A post holds `post.lock` in the PR's directory while it runs; a second one fails with `already posting #7 in another run`.

## Last reviewed head

The plugin owns it. A `post` that finishes, having posted a comment or submitted the review, records the session's head, the one it posted against, as `last_reviewed_head`, and says so in its answer; `pull` leaves it. Until a post records one, it's the commit of your last review submitted on GitHub (among the PR's last 20), when the clone has that commit. "Since last review" compares with it, however many updates came since.

## Detecting

`detect` keeps the PR's branch in its session, and answers from the newest session kept for the branch checked out without asking GitHub. Else it asks for the repository's open PR from that branch, in one query. When GitHub has none, that's kept (`detect.json`) for 10 minutes, or until the branch's commit or the branch changes, and `detect` answers `{"review":null}` from it without asking.

## Watching

While dffy shows a PR and you aren't idle, the plugin asks GitHub's REST API about it every 60 seconds, and at once when you're back, never git, sending the `ETag` of its last answer as `If-None-Match`: GitHub answers 304 when nothing changed, which doesn't count against the rate limit. It pushes:

- `head/moved` when the PR's head isn't the session's, and again when that changes;
- `comments/changed` once the PR's `updated_at` differs from what the last `prepare` or `pull` imported, as a new comment makes it (a push, an edit of the PR or your own post changes it too);
- a `log` the first time asking fails, and again only once it fails another way.

It waits while a `prepare` or `pull` runs.

## Commands of its own

- **`dffy-review-github prefetch`** is yours to run, not dffy's. It fetches the heads of the PRs `list` shows, so preparing one later waits only for its threads, and prints `{"fetched":[7,9]}`.
- **`dffy-review-github prune [--days <n>]`** is yours to run too. It drops what it keeps of PRs GitHub last said were merged or closed and that weren't prepared or pulled for `n` days (30 without `--days`), with their `refs/dffy/pull/<pr>`, and prints `{"pruned":[3,5]}`. Their comments are dffy's, and stay. A PR whose old `comments.json` holds a comment without a `ref`, or can't be read, stays ([Storage](#storage)). `list` and `detect` run it once answered, at most once a day. A PR merged since it was last prepared still reads as open, and stays. A `prune` request, with `days`, does the same; dffy sends none.

## Its words and fields

- **Its words.** It describes `dffy pr`, "pull request", `PR #7` (its ids are PR numbers, and its labels left out), the events `p` pending (`"submits":false`), `c` comment, `a` approve and `r` request changes, and the warning that GitHub collapses outdated threads, so a reply there is easy to miss. Its sessions name no labels to vote on. Its `list` groups by `current_branch`, `review_requested` and `mine`.
- **Its fields.** It keeps `github_thread`, a thread's id on GitHub, and `github_unplaced` on a thread's first comment.

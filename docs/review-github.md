# dffy-review-github

`dffy-review-github` is the review plugin for GitHub pull requests. This page is for anyone reviewing PRs in dffy or working on the plugin, and covers the plugin's side. [plugins.md](plugins.md#review-plugins) covers enabling it and the protocol.

**Experimental.** It may change with the [review protocol](plugins.md#the-review-protocol-version-1).

dffy opens and draws the PR. The plugin talks to GitHub and git. Run it in a clone with a remote on GitHub.

## The repository and the host

- **The repository.** The plugin takes the remote `gh` takes: the one `gh repo set-default` marked (`remote.<name>.gh-resolved`), else `upstream` when it's on a GitHub host, as in a fork's clone, else `origin`. URLs are read as git rewrites them (`url.<base>.insteadOf`). The plugin fetches from that remote, and its URL names the host, owner and repository.
- **Hosts.** A host is GitHub's when it's github.com, `GH_HOST`, or one `gh auth status --hostname <host>` lists. gh answers that without asking a host it has no login for. The answer is kept in `host.json` for a day, or until gh's program or its `hosts.yml` changes. Every `gh api` call names the host with `--hostname`. Without such a remote, `detect` answers `{"review":null}` without touching the network, and the other requests fail naming the remote and host they found.
- **`gh`.** The plugin reaches GitHub only through the [GitHub CLI](https://cli.github.com), logged in to the repository's host. `doctor` checks that host, or outside a repository gh's own host, `GH_HOST` or github.com. It runs `gh --version` and `gh auth status` side by side, killing either after 2 seconds and failing its row (`` `gh auth status` didn't answer in 2s ``). Requests don't check gh first. The first one that calls it finds out:
  - without `gh` on `PATH`: `dffy-review-github needs the GitHub CLI (gh): https://cli.github.com`;
  - logged out, or with a token GitHub refuses: ``gh isn't logged in to github.com; run `gh auth login` ``.

  A `prepare` that has to ask GitHub then sends no session. One answered from its kept session still opens, and the error follows.
- **Rate limits.** When the rate limit is used up, the error says when it resets (`resets at 14:05 UTC`). The time comes from the answer's `X-RateLimit-Reset`, or for GraphQL from the `rate_limit` endpoint, which costs no rate limit. A secondary limit shows GitHub's message alone.

## Storage

The PR's comments belong to dffy ([comments.md](comments.md#where-the-file-is)). The plugin keeps these under `$XDG_DATA_HOME/dffy-review-github/<repository>/<pr>`:

- the PR's session;
- GitHub's hunks of the PR at the session's head;
- its watcher's last answer from GitHub (`check.json`).

Each repository's directory also holds `host.json`, `detect.json` (the branch `detect` last found no PR for) and `pruned` (when `prune` last ran).

A `comments.json` in a PR's directory is the PR's comments file from before dffy kept them. The plugin no longer reads it. Import it with `dffy comments --session import <that file>` once the PR is open ([comments.md](comments.md#importing-dffy-comments-import)), then remove it. While it holds a comment without a `ref`, or can't be read, `prune` keeps the PR's directory.

## Opening

The code comes only through `git fetch`, as the ref `refs/dffy/pull/<pr>`.

- A PR prepared before, whose commits are still in the clone, opens from its session before the plugin asks GitHub anything. Its threads download after that. When GitHub's first page shows another head than the session's, the plugin pushes `head/moved` before the progress.
- dffy shows the session's head until a `pull`, even after the PR moves on. The watcher reports when it has.
- When GitHub can't be reached, the code still opens on the fetched head, with the session's base or the default branch's tip. The threads stay as they were.
- The session's `checkout` is `gh pr checkout <pr>`. dffy shows it where it needs the PR checked out, as to apply a suggestion.

## Threads

Threads merge into the file by `ref`, and drafts are never touched. A thread you resolved or reopened in dffy keeps your state until it's posted. Each comment gets its `url`, and a thread GitHub marks outdated gets `outdated`.

A thread's lines and anchor are read from the commit it was written on, outdated or not, or for the old side from that commit's merge base with the base. GitHub's placement on the current head is ignored. dffy places the thread by its anchor's text.

- A force-push can leave an original commit on no ref, so the head's fetch doesn't bring it. After the last page, the plugin fetches the missing commits by oid in one `git fetch`, onto no ref, and places their threads. Until then, and for good if GitHub no longer has the commit, the thread is a file comment with `"github_unplaced":"original commit not available"`. A commit GitHub lacks fails the whole fetch, so it's retried without the one git names.
- A placed thread keeps its lines even if its commit later leaves the clone, so the commit is never fetched again for it. A commit GitHub dropped is asked for again on each `prepare` or `pull`, after the session line.
- A comment in your pending review on GitHub comes in with `"pending":true`, whether posted with the `pending` event or on GitHub's site. The mark goes once the review is submitted.
- A comment GitHub no longer has leaves the file once every page is read, by dffy's rules for [`gone`](comments.md#gone). A thread holding your drafts stays. Every comment of your pending review counts as present.

## Conversation

The PR's [general comments](comments.md#general-comments) come in the same requests as its threads, one page of each list at a time. A later page asks only for the lists that have more. They merge by `ref` too, each with its `url`. The viewer shows them as the review's [Conversation](viewer.md#conversation).

- The PR's description, `"kind":"description"`, written by the PR's author when the PR was opened. An empty one is left out.
- Each comment in the PR's conversation, `"kind":"comment"`.
- The summary of each submitted review, `"kind":"review"`, dated when it was submitted, with its state as its `label`: `approved`, `changes requested`, `commented` or `dismissed`. A review without a summary is left out, as one with only line comments. So is your pending review, whose summary isn't sent yet.
- Progress lines count threads only.
- One GitHub no longer has, or whose text was emptied, leaves the file once every page is read, as deleted threads do. One you replied to in a draft stays without its `ref`, and the draft isn't posted.

## Posting

- Each comment is its own request, paced to stay under GitHub's limit. Its `ref` goes to dffy as soon as it's posted, so posting again never sends a comment twice, even after a crash partway.
- A post runs alone. It waits for a running `prepare` or `pull`, which dffy cancelled, and none starts until it ends. Dry runs, `list` and `detect` run beside anything.
- Comments are posted against the session's head, even after the PR moves on.
- Each draft is planned from git and the hunks `prepare` or `pull` kept, without asking GitHub. `dryRun` returns the same plan and sends nothing.
  - A draft goes where its anchor's lines are in the file at that head (at its merge base with the base, for the old side), not at the line numbers it was written on. A draft written on the working tree, with lines added above it, moves with its lines. One whose lines aren't at the head is skipped: `lines not in the PR head (unpushed changes?)`.
  - A draft on a file the PR doesn't change is skipped: `file not in the PR`.
  - A line comment goes on its lines only when its first and last line at the head fall in one hunk of GitHub's diff of the PR. GitHub refuses it otherwise, so it becomes a file comment that quotes the lines. `prepare` and `pull` fetch the PR's files beside its threads and keep their hunks for the session's head.
  - Without GitHub's hunks for the session's head, or when GitHub left a file's patch out (too large, or past the 3000 files it lists), git's diff of the PR decides: `base...head`, 3 lines of context, two hunks one line apart joined as GitHub joins them. The plan then says `"unsure":true`, since git can still draw a hunk's edges where GitHub doesn't. The draft's `distance` was measured on the diff it was written on, so it's used only when git can't diff the PR either.
  - A reply goes in its thread when the thread is on GitHub or its first comment is planned to post first. Otherwise it's skipped: `its thread isn't posted`.
- Only your drafts are posted. A comment without a `ref` whose `author` isn't empty, such as an agent's, is neither planned nor counted.
- Each posted comment goes in your pending review, marked `"pending":true` until the review is submitted.
- A skipped draft stays a draft, its reason in the answer's `unposted`. The review is still submitted with the event, since dffy showed the skips before you sent.
- A draft GitHub refuses while posting stays a draft too, with GitHub's reason in its `post_error`, which goes once it's posted. The review is still submitted without it.
- Only GitHub no longer answering stops a post. The event isn't applied, the error says so, and the next post sends the rest. The file's `notice` holds only what concerns the whole run: a stop, a failed submit, resolves GitHub refused.
- A post holds `post.lock` in the PR's directory. A second one fails with `already posting #7 in another run`.

## Last reviewed head

The plugin owns it. A `post` that posts a comment or submits the review records the session's head, which it posted against, as `last_reviewed_head`, and says so in its answer. `pull` leaves it. Until a post records one, it's the commit of your last review submitted on GitHub, among the PR's last 20, when the clone has that commit. "Since last review" compares with it, however many updates came since.

## Detecting

`detect` keeps the PR's branch in its session, and answers from the newest session kept for the checked-out branch without asking GitHub. Otherwise it asks for the repository's open PR from that branch, in one query. When there's none, that's kept in `detect.json` for 10 minutes, or until the branch or its commit changes, and `detect` answers `{"review":null}` from it.

## Watching

While dffy shows a PR and you aren't idle, the plugin asks GitHub's REST API about it every 60 seconds, and at once when you come back. It never asks git. It sends the `ETag` of its last answer as `If-None-Match`, so GitHub answers 304 when nothing changed, which doesn't count against the rate limit. It pushes:

- `head/moved` when the PR's head isn't the session's, and again when that changes;
- `comments/changed` once the PR's `updated_at` differs from what the last `prepare` or `pull` imported. A new comment changes it, and so does a push, an edit of the PR or your own post;
- a `log` the first time asking fails, and again only when it fails another way.

It waits while a `prepare` or `pull` runs.

## Commands of its own

You run these, not dffy.

- **`dffy-review-github prefetch`** fetches the heads of the PRs `list` shows, so preparing one later waits only for its threads. It prints `{"fetched":[7,9]}`.
- **`dffy-review-github prune [--days <n>]`** drops what the plugin keeps of PRs GitHub last called merged or closed and that weren't prepared or pulled for `n` days (30 without `--days`), with their `refs/dffy/pull/<pr>`. It prints `{"pruned":[3,5]}`. Their comments are dffy's and stay. A PR whose old `comments.json` holds a comment without a `ref`, or can't be read, stays ([Storage](#storage)). A PR merged since it was last prepared still reads as open, so it stays too. `list` and `detect` run it after answering, at most once a day. A `prune` request with `days` does the same, though dffy sends none.

## Its words and fields

- **Words.** It describes `dffy pr`, "pull request" and `PR #7`. Its ids are PR numbers, with no `label`. Its events are `p` pending (`"submits":false`), `c` comment, `a` approve and `r` request changes. It warns that GitHub collapses outdated threads, so a reply there is easy to miss. Its sessions name no labels to vote on. Its `list` groups by `current_branch`, `review_requested` and `mine`.
- **Fields.** It keeps `github_thread`, a thread's id on GitHub, and `github_unplaced` on a thread's first comment.

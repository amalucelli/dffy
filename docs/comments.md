# Review comments

**Experimental.** The file format and the operations may change.

dffy keeps review comments in a JSON file: comments on lines of a diff, replies to them, and whether a thread is resolved. The viewer draws them, and integrations such as a [review plugin](plugins.md#review-plugins) keep the file in step with a code host.

- A running viewer reads the file again whenever another program writes it, so a comment written elsewhere shows up in it:
  - It watches the file's directory with the OS's file watcher (FSEvents on macOS, inotify on Linux), so a write that renames a new file over the old one, as `dffy comments apply` does, counts. Nothing is polled while the file is unchanged; on Linux, when the inotify watch limit is reached, the file is checked once a second instead.
  - It also reads the file when a review plugin it runs reports progress, hands over a review, or exits: plugins write as they go.
  - A burst of writes is read once, and a file that stat says hasn't changed since the last read, as after the viewer's own writes, isn't read again.
- Only dffy writes it: the viewer, and `dffy comments apply` for everyone else. An integration sends [operations](#writing-dffy-comments-apply) and dffy applies them under a lock, so two writers never lose each other's changes.
- dffy never talks to the network. Posting comments to a host and pulling them back is the integration's job.

## Where the file is

- `--comments <file>` picks the file, for every subcommand. An integration keeps one per review in its own storage and passes it when it starts dffy. A relative path is relative to the directory dffy runs in, which `-C` sets.
- Without it, the file is `dffy/comments.json` in the git directory every worktree of the repository shares (`.git/dffy/comments.json`, from a linked worktree too), for reviewing your own changes. Outside a repository there's no default, and `dffy comments` fails until `--comments` names a file.
- `dffy comments --session` reads the file of the review session the viewer has open, which the [session pointer](#the-session-pointer) names.
- On a code host's review ([viewer.md](viewer.md#code-host-reviews)), the viewer shows the integration's file and, beside it, your drafts in the file plain `dffy` uses (your comments without a `ref`, with their replies) that place on the review's diff. Changes to those drafts go to their own file, and so do new comments on the review plain `dffy` found for the branch, which are the working tree's until posted. Posting them (the post dialog's `l`) moves them into the integration's file first: each is added there as a new comment, with a new `id`, its replies after it, and its `branch` dropped, and removed from its own file once the other has it.
- A file that doesn't exist reads as no comments. The first write creates it, and its directory. The viewer creates the directory when it starts, to watch it.
- A diff piped to dffy has no comments.

### Branches

One file holds the local comments of every branch and worktree, each thread on the branch it was written on (its `branch`):

- **The current branch** is HEAD's. While a rebase stops (on a conflict, or to edit a commit), HEAD is detached and the current branch is the one being rebased, as git records it in the worktree's `rebase-merge/head-name` or `rebase-apply/head-name`. On any other detached HEAD there's no current branch.
- **Whatever is shown:** `dffy show <commit>`, `dffy main..HEAD` and every other diff take the current branch too. A comment belongs to the branch you were on when you wrote it, not to the commits on screen.
- **Shown:** the current branch's threads, and those on no branch. A thread on no branch, such as one written on a detached HEAD or before comments had branches, shows on every branch. On a detached HEAD only those show.
- **Other branches:** a thread whose branch you aren't on, renamed or deleted branches included, is on another branch. `dffy comments --all-branches` lists them all; `set` with a new `branch` moves a thread to another branch, or with `null` to none. The viewer lists them last in its comments panel, folded to one row until `b` ([viewer.md](viewer.md#finding)).
- **Written:** the viewer gives a thread it starts in this file the current branch, and so does `dffy comments apply` to a thread `add` starts without one ([`add`](#operations)).

### Moving from the worktree's file

Before branches, a linked worktree kept its own file, `dffy/comments.json` in its own git directory (`.git/worktrees/<name>/dffy/comments.json`). The first time dffy opens the local file from that worktree, it moves the old file to the shared place, under the shared file's lock, when the shared one doesn't exist yet. Once it does, another worktree's old file stays where it was: to keep its comments, apply them to the shared file with `dffy comments apply`. The moved comments have no `branch`, so they show on every branch.

### The session pointer

`dffy/session` in the shared git directory names the review session the viewer has open, so that `dffy comments --session` and agents ([mcp.md](mcp.md#list_comments)) find its comments without asking the host. The viewer writes it when it opens or finds a review, and it stays after the viewer exits, until another session replaces it:

```json
{"version":1,"comments":"/home/ada/.local/state/dffy-review-github/acme/widgets/7.json","review":"7","branch":"feature/x","head":"e4f5a6b"}
```

| Field | Meaning |
|---|---|
| `version` | `1`. |
| `comments` | The session's comments file, an absolute path. |
| `review` | The review plugin's id for the review. |
| `branch` | The [current branch](#branches) when it was opened; `null` on a detached HEAD. |
| `head` | The review's head commit. |

Without one, `dffy comments --session` fails and says so; a pointer that isn't this shape is an error, not an empty session.

### Drafts

A **draft** is a comment of yours (an empty `author`) without a `ref`, in a review session's file, the one a review plugin keeps for a review; `P` posts it. Elsewhere nothing is posted, so the viewer shows no drafts there and doesn't count any.

A comment without a `ref` whose `author` isn't empty, such as one an agent wrote, is never a draft: posting it would put it out under your name. In a review session it shows `not posted`, stays out of the counts and of what `P` sends, and a review plugin's `post` leaves it alone. You can still edit or delete it, or make it yours with `set` and an empty `author` (`a` on its thread in the viewer), which makes it a draft.

## The file, version 1

```json
{"version":1,"notice":"comments as of 10:42, refresh failed","comments":[
{"id":"9f3c2a71b04e5d18","path":"src/lib.rs","side":"new","start":{"old":12,"new":12},"end":{"old":null,"new":14},"text":"This can overflow.","author":"","created":"2026-11-17T10:42:00Z","resolved":false,"anchor":{"before":["pub fn add(a: i32, b: i32) -> i32 {","    let x = a;"],"lines":["    let y = b;","    let sum = x + y;","    sum"],"after":["}",""]},"distance":0},
{"id":"1b7e94c0d25a6f03","path":"src/lib.rs","text":"Fixed, thanks.","author":"octocat","created":"2026-11-17T11:03:12Z","in_reply_to":"9f3c2a71b04e5d18","resolved":false,"ref":"github:PRRC_kwDOA"}
]}
```

dffy writes one comment per line, but readers must not rely on the layout.

| Field | Meaning |
|---|---|
| `version` | The format version, `1`. |
| `notice` | Optional. A line an integration sets for the viewer to show in its status bar when it reloads the file, such as `comments as of 10:42, refresh failed`. |
| `summary` | Optional, in a review session's file: the review's summary as you're writing it, before it's posted with the review. The viewer writes it as its post dialog closes, and removes it once a post that submits the review ends well. |
| `comments` | The comments, in the order they were added. |

A comment:

| Field | Meaning |
|---|---|
| `id` | A string dffy assigns when the comment is added. It never changes and is never given to another comment, even after this one is removed. Treat it as opaque. |
| `path` | The file's path on the new side, relative to the repository root, as in [json.md](json.md#files). Left out for a [general comment](#general-comments). |
| `old_path` | The path on the old side, for a renamed file only. A comment on the old side's lines shows on the file with this path when a diff lists it apart from `path`'s, as one that doesn't pair the rename does. |
| `side` | `old` or `new`: the side of the last commented line. |
| `start_side` | `old` or `new`: the side of the first commented line. When it's left out, it's `side`. |
| `start`, `end` | The first and last commented lines, each `{"old": <n>, "new": <n>}`: the line's number on each side, 1-based, or `null` on a side it isn't on, as a line in [json.md](json.md#lines). A context line has both numbers, an added line only `new`, a removed line only `old`. Left out for a comment on the whole file. |
| `text` | The comment, as Markdown: `CommonMark` with GitHub's tables, strikethrough and task lists, which GitLab and Gerrit read too. The viewer draws it rendered ([viewer.md](viewer.md#reading)), a line break kept as one and HTML comments hidden, so an integration passes a host's text through as it is. A `suggestion` fenced block left in the text shows as code; the viewer draws a suggested change from `suggestion`. |
| `suggestion` | Optional replacement text for the commented lines, without a final newline. |
| `author` | Who wrote it, a free string. Empty means you; only your comments are [drafts](#drafts). |
| `created` | When it was written, RFC 3339 (`2026-11-17T10:42:00Z`). dffy writes UTC; integrations give their host's timestamp. |
| `in_reply_to` | For a reply, the `id` of its thread's first comment. |
| `resolved` | Whether the thread is resolved. It's read from the thread's first comment. |
| `ref` | Opaque, owned by an integration: the comment's identity on a host, such as dffy-review-github's `github:<node id>`. A comment with a `ref` has been posted. Each `ref` is on one comment at most. |
| `anchor` | The text of the commented lines, with up to 2 lines of context on each side. See [Anchors](#anchors). |
| `distance` | How many lines the commented lines are from the nearest change on their side, 0 when they include one, in the diff the comment was written on. A hint for hosts that accept a comment on lines only near a change, whose diff may not be that one. Left out when it isn't known. See [Distance](#distance). |
| `url` | The comment on its host, set by an integration. `o` on a focused thread in the viewer opens it; only `http` and `https` links open. |
| `outdated` | `true` when the host marks the thread outdated (its lines changed since). Read from the thread's first comment; left out when false. The viewer shows it, and before you reply to one says the review plugin's warning about such threads, when it has one. |
| `kind` | Optional, for a general comment: what it is, `description`, `review` or `comment`, set by the integration. See [General comments](#general-comments). |
| `label` | Optional, for a general comment: a word the integration gives it, such as a review's verdict (`approved`). dffy shows it as given. |
| `branch` | Optional, for a local comment: the [branch](#branches) it was written on. It's the thread's: read from its first comment, and a reply added to a thread takes it. Left out for a thread that shows on every branch. |
| `pending` | `true` when the comment is on the host but visible only to you, as in a review not submitted yet. Set by an integration; left out when false. |
| `post_error` | Optional: why the host refused the comment when it was posted, set by an integration and cleared when a post goes through. `dffy comments` shows it under the comment. |

- `id`, `text`, `author`, `created` and `resolved` are always present, and so is `path` on every comment but a general one. dffy leaves out the other fields when they don't apply; a reader treats a field that's `null` as left out.
- **Kinds of comment:**
  - A **line comment** has `start`, `end`, `side` and `anchor`.
  - A **file comment** has none of them.
  - A **general comment** has no `path` either.
  - A **reply** has `in_reply_to`, and its thread's `path` and `old_path`; its place is its thread's. It has no lines or anchor of its own.
- **Text:** a line of `anchor` is the line without its `\n`; a `\r` before it stays. A line that isn't valid UTF-8 has each invalid sequence replaced by U+FFFD, and an anchor taken from it only matches the same replaced text.

### General comments

A general comment is on the review as a whole, not on a file: a pull request's description, a comment in its conversation, the summary submitted with a review. Other hosts have the same, as a merge request's notes or a change's messages.

```json
{"id":"5d0e7b13a9c2f486","text":"Looks good, one question inline.","author":"hubot","created":"2026-11-17T12:30:00Z","resolved":false,"ref":"github:PRR_kwDOA","url":"https://github.com/acme/widgets/pull/7#pullrequestreview-2001","kind":"review","label":"approved"}
```

- It has no `path`, `old_path`, `side`, lines or anchor.
- Only an integration adds one, importing it from a host, so it has a `ref`: `add` and `upsert` refuse a comment without a `path` and without a `ref`, and one without a `path` that has lines. dffy doesn't write general comments yet.
- `kind` says what it is, in words that aren't any host's: `description` for the review's own description, `review` for the summary of a submitted review, `comment` for any other. A reader shows a kind it doesn't know as a comment.
- `label` is the integration's word for it, shown as given; dffy-review-github gives a review's state, such as `approved` or `changes requested`.
- A reader that doesn't know general comments reads one as a comment on no file, and keeps its `kind` and `label` as fields it doesn't know.
- The viewer shows a review session's general comments as its [Conversation](viewer.md#conversation).

### Anchors

```json
{"before":["pub fn add(a: i32, b: i32) -> i32 {","    let x = a;"],"lines":["    let y = b;","    let sum = x + y;","    sum"],"after":["}",""]}
```

| Field | Meaning |
|---|---|
| `lines` | The commented lines, `start` to `end` on `side`. At least one. |
| `before`, `after` | Up to 2 lines before and after them on that side; fewer at the start or end of the file. |

Line numbers alone go stale: a host moves a thread to a newer commit, and a local file changes under its comments. So dffy places a line comment by its anchor, against the text of its side:

1. **Exact:** the lines at `start` to `end` are the anchor's `lines`. The comment stays there.
2. **Moved:** the anchor's `before`, `lines` and `after`, in a row, appear exactly once within 1000 lines of `start`. The comment goes to that copy of `lines`.
3. **Unplaced:** otherwise. The viewer shows the comment under its file's last row, headed with the lines `start` and `end` name on `side`, over the anchor's lines and context as they were; a commented line shows as unchanged when the comment numbers it on both sides. `i`'s list has it apart, with its file.

A range whose first line is on the other side (`start_side` isn't `side`, as when it runs from removed lines to added ones) is anchored and placed by its lines on `side`: from `start`'s number on `side` when it has one, else `end` alone.

The viewer fills in anchors for the comments you make in it. An integration fills them in from git, from the file at the commit the host gives for each comment, not from the diff on screen.

### Distance

`distance` counts lines on the comment's side from the commented lines to the nearest change in the diff:

- A changed line on that side (added on `new`, removed on `old`) is as far as its number is from the nearest commented line, and 0 when it's one of them.
- A change on the other side sits between two lines of this side. Both of those lines are 1 from it, as when lines were only removed and the comment is on the `new` side next to where they were.

The viewer writes it when you add a comment, from the diff on screen, which may be your uncommitted changes rather than the review's. So it's a hint, for hosts that accept a comment on lines only near a change: an integration that can diff what the host shows measures the lines against that instead, and uses `distance` only when it can't. What it does with a comment too far from a change is up to it; dffy-review-github posts it as a file comment, quoting the lines.

### Versions

- A reader ignores fields it doesn't know, at the top level and in a comment, and dffy keeps them when it rewrites the file. Only those two levels: unknown fields inside `start`, `end` or `anchor` are dropped. An integration can store its own fields there; prefix them with its name (`github_thread`) to keep clear of fields dffy may add.
- New fields keep the version. Removing or renaming a field, or changing its type or meaning, bumps it.
- dffy refuses a file of a version it doesn't know, and never rewrites one.

## Reading: `dffy comments`

```sh
dffy comments                     # this branch's threads, for reading
dffy comments --open --path src/lib.rs
dffy comments --all-branches      # every branch's
dffy comments --markdown          # the threads as Markdown, to paste or hand on
dffy comments --json              # the whole file, for scripts
dffy comments --session           # the review session the viewer has open
dffy --comments pr.json comments  # another file
dffy comments rm --resolved       # remove this branch's resolved threads
```

`dffy comments` lists the comments as threads. Each thread starts with its file and lines (`src/lib.rs:12-14`, `(old side)` for lines on the old side, nothing more for a file comment), `branch <name>` when it's on a branch other than the one listed, and `resolved` when it is. Then come its comments in the order they were added, each with its `id`, author (`you` when it's empty), time, `posted` when it has a `ref` and `pending` when it's `pending`, over its text and, with a `post_error`, `not posted:` and the reason. Threads go by path, then by first line. General comments come before them, each headed `conversation` with its `kind` and `label` (`conversation, review, approved`). The file's `notice`, when it has one, comes first, then its `summary`.

Which threads it lists, each judged by its first comment, so a thread is listed or left out whole:

| Flag | Lists |
|---|---|
| (none) | The [current branch](#branches)'s threads and those on no branch. Outside a repository, with `--comments`, every thread. When threads on other branches are left out, a last line says how many. |
| `--branch <name>` | That branch's threads and those on no branch. |
| `--all-branches` | Every branch's threads. |
| `--open`, `--resolved` | Only open threads, or only resolved ones. |
| `--path <path>` | Only the threads on that file, relative to the repository root; a renamed file's old path works too. General comments are on no file. |

For [the file above](#the-file-version-1):

```
notice: comments as of 10:42, refresh failed

src/lib.rs:12-14
  9f3c2a71b04e5d18 you, 2026-11-17T10:42:00Z
    This can overflow.
  1b7e94c0d25a6f03 octocat, 2026-11-17T11:03:12Z, posted
    Fixed, thanks.
```

`dffy comments --json` prints the whole file in [its format](#the-file-version-1), as dffy would write it, every branch's comments included, and never colored or paged; it takes none of the flags that pick threads. A file that doesn't exist prints as an empty one, `{"version":1,"comments":[]}` (with a line break in the list). Scripts read the file this way rather than opening it, so they never see a version they don't know: dffy refuses it and exits with status 128.

`dffy comments --markdown` prints the threads it would list as Markdown, never colored or paged: the `summary` under `## Summary` when there is one, then a `##` heading per thread with its file, lines and state, the commented lines as they were when it was written (its `anchor`) in a code block, and each comment as its author and time in bold over its text, which is Markdown already, with its suggestion in a code block after it. Each code block's fence is longer than any run of backticks in it.

`dffy comments --session` reads the file the [session pointer](#the-session-pointer) names instead of the local one, without asking the host, so it shows what the review plugin last wrote. It can't be given with `--comments`, and fails when no session was opened in the repository.

`dffy comments rm --resolved` removes the resolved threads the listing would show, each with its replies, under the file's lock, and prints how many comments went: the current branch's and those on no branch, or with `--branch <name>` that branch's and those on none, or with `--all-branches` every resolved thread. `--resolved` is required, so a bare `rm` removes nothing.

`comments` is a subcommand, so a revision named `comments` needs the explicit form: `dffy diff comments`.

Agents read the same comments through the MCP tool `list_comments` ([mcp.md](mcp.md#list_comments)).

## Writing: `dffy comments apply`

```sh
echo '[{"op":"add","comment":{"path":"README.md","text":"Typo in the title."}}]' | dffy comments apply
```

`dffy comments apply` reads a JSON list of operations on stdin, which mustn't be a terminal, and applies them to the file in one write:

1. It takes an advisory lock, `flock` on `<file>.lock` beside the file, waiting while another writer holds it.
2. It reads the file as it is now.
3. It applies the operations in order.
4. It writes the result to a temporary file in the same directory and renames it over the file.
5. It releases the lock.

The operations are all or nothing: when one fails, nothing is written. Batch everything one change needs into one call, such as a page of imported threads, so other writers see it whole.

### Operations

| Operation | Does |
|---|---|
| `{"op":"add","comment":{…}}` | Adds a comment. dffy assigns its `id`. |
| `{"op":"set","id":"…","fields":{…}}` | Changes some fields of the comment with that `id`. `"ref":"…"` instead of `"id"` picks the comment with that `ref`. |
| `{"op":"upsert","comment":{…}}` | For imports: updates the comment with the given comment's `ref`, or adds it when there's none. |
| `{"op":"rm","id":"…"}` | Removes the comment with that `id` (or `"ref":"…"`), and its replies when it starts a thread. |
| `{"op":"notice","notice":"…"}` | Sets the file's `notice`; `null` removes it. |
| `{"op":"summary","summary":"…"}` | Sets the file's `summary`; `null` removes it. |

**`add`.** The comment has the fields of [the file](#the-file-version-1), except:

- no `id`;
- `created` may be left out for now;
- `author` may be left out for you, and `resolved` for false;
- `side` may be left out for `new`;
- a reply names its thread with `in_reply_to`, the `id` of any comment in the thread, or with `in_reply_to_ref`, its `ref`. That comment must be in the file already or added earlier in the same list. A reply needs only `text`; it takes its thread's `path`, `old_path` and `branch`, and any lines, anchor or branch it's given are dropped.

On the local review's file (no `--comments` or `--session`), a thread `add` starts without a `branch` field goes on the [current branch](#branches); `"branch":null` keeps it on none, so it shows on every branch. On any other file, such as an integration's, `add` doesn't fill it in. A reply always takes its thread's.

A line comment needs `start`, `end` and an `anchor` with at least one line; `start` needs a number on `start_side`, and `end` on `side`. A file comment needs `path` and `text`. A [general comment](#general-comments) needs `ref` and `text`, and no lines.

**`set`.** `fields` holds any of:

| Field | Sets |
|---|---|
| `text` | The text. Refused on a comment with a `ref`, unless the same `set` removes it: it's posted, and its text is the host's. |
| `suggestion` | The suggestion; `null` removes it. Refused on a comment with a `ref`, as `text` is. |
| `author` | Only `""`: the comment becomes yours, as when you adopt one an agent wrote, which makes it a [draft](#drafts) in a review session. Refused on a comment with a `ref`, as `text` is. |
| `resolved` | Whether the thread is resolved; set it on the thread's first comment. |
| `ref` | The `ref`, after posting the comment; `null` removes it. |
| `anchor` | The anchor, for a comment placed again on newer text. |
| `branch` | The thread's [branch](#branches), whichever of its comments the `set` names: it moves the whole thread there, or with `null` to no branch. |
| `pending` | Whether it's on the host only for you. |
| `post_error` | Why the host refused it; `null` removes it. |

**`upsert`.** The comment needs a `ref` and has no `id`, and can name its thread with `in_reply_to` or `in_reply_to_ref` as in `add`. A comment read from the file is upserted back without its `id`.

- When a comment has that `ref`, the fields given replace its own, `text` included; the fields left out keep their values, and so do its `id` and the fields dffy doesn't know. A field given as `null` is removed.
- Otherwise it's added, as by `add`.

### The answer

On success, dffy prints one JSON object on stdout:

```json
{"ids":["e2a05d7c1f9b4836",null,"9f3c2a71b04e5d18"]}
```

`ids` has an entry per operation, in order: the `id` of the comment it added, changed or removed, or `null` for `notice` and `summary`.

On failure, dffy prints the error on stderr and exits with status 128, and the file is unchanged. The message starts with the failing operation's position in the list, from 0:

```
error: operation 1 (set): comment 77c1e0a94b2d5f60 doesn't exist
```

An operation fails when:

- it's malformed: an unknown `op`, a field of the wrong type, or a comment missing what it needs;
- the comment it names doesn't exist;
- it would give a `ref` to a second comment;
- it sets `text`, `suggestion` or `author` on a comment with a `ref`, or `author` to anything but `""`;
- the file is of a version dffy doesn't know, or isn't valid JSON.

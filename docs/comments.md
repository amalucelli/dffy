# Review comments

dffy keeps review comments in a JSON file: comments on lines of a diff, their replies, and whether each thread is resolved. `dffy comments` reads the file and `dffy comments apply` writes it.

The file format and the operations are experimental and may change.

```sh
dffy comments                     # this branch's threads, for reading
dffy comments --open --path src/lib.rs
dffy comments --all-branches      # every branch's
dffy comments --markdown          # the threads as Markdown, to paste or hand on
dffy comments --json              # the whole file, for scripts
dffy comments --session           # the review session the viewer has open
dffy --comments pr.json comments  # another file
dffy comments rm --resolved       # remove this branch's resolved threads
dffy comments prompt 9f3c2a71b04e5d18  # one thread as a prompt for a coding agent
dffy comments listen --as claude       # print what viewers hand over, for a running agent
```

The viewer draws the comments. Printed output and `--json` show them with `--show-comments` ([usage.md](usage.md#review-comments-in-printed-output)). A [review plugin](plugins.md#review-plugins) keeps the file in step with a code host.

- Only dffy writes the file: the viewer, and `dffy comments apply` for every other program. Integrations send [operations](#writing-dffy-comments-apply), and dffy applies them under a lock, so concurrent writers never lose each other's changes. A review plugin sends the same operations over its session with dffy.
- A running viewer reloads the file whenever another program writes it.
- dffy never talks to the network. Posting comments to a host and pulling them back is the integration's job.

## Reading: `dffy comments`

`dffy comments` lists the comments as threads:

```
notice: comments as of 10:42, refresh failed

src/lib.rs:12-14
  9f3c2a71b04e5d18 you, 2026-11-17T10:42:00Z
    This can overflow.
  1b7e94c0d25a6f03 octocat, 2026-11-17T11:03:12Z, posted
    Fixed, thanks.
```

- The file's `notice` comes first, then its `summary`, when they exist.
- [General comments](#general-comments) come next, each headed `conversation` with its `kind` and `label` (`conversation, review, approved`).
- Threads follow, by path, then by first line. A thread's heading is its file and lines (`src/lib.rs:12-14`), plus `(old side)` for old-side lines. A file comment's heading is just the file. The heading adds `branch <name>` for a thread on another branch than the one listed, and `resolved` when it is.
- Each comment shows its `id`, its author ([`you`](#who-you-is) when empty), its time, `posted` when it has a `ref`, and `pending` when it's `pending`. Its text follows, indented, control characters and Unicode's line separators shown by name. A comment with a `post_error` adds `not posted:` and the reason.

Filters judge a thread by its first comment, so a thread is listed or left out whole:

| Flag | Lists |
|---|---|
| (none) | The [current branch](#branches)'s threads and those on no branch. Outside a repository, with `--comments`, every thread. A last line counts the threads left out on other branches. |
| `--branch <name>` | That branch's threads and those on no branch. |
| `--all-branches` | Every branch's threads. |
| `--open`, `--resolved` | Only open threads, or only resolved ones. |
| `--path <path>` | Only threads on that file, relative to the repository root. A renamed file's old path works too. General comments are on no file. |

Other forms:

| Command | Output |
|---|---|
| `dffy comments --json` | The whole file in [its format](#the-file-version-1), as dffy would write it, every branch included. Never colored or paged. Takes none of the filter flags. A missing file prints as `{"version":1,"comments":[]}` (with a line break in the list). Scripts should read the file this way, not open it: a version dffy doesn't know exits with status 128. |
| `dffy comments --markdown` | The threads the listing would show, as Markdown, never colored or paged. The `summary` comes first under `## Summary`. Each thread gets a `##` heading with its file, lines and state, then its `anchor` lines in a code block, then each comment as its author in bold and its time over its text, with any suggestion in a code block. The lines and the comments are quoted (`>`). Every fence is longer than any run of backticks inside it. |
| `dffy comments --session` | The file the [session pointer](#the-session-pointer) names, instead of the local one: what the review plugin last wrote, without asking the host. Can't be combined with `--comments`. Fails when no session was opened in the repository. |
| `dffy comments rm --resolved` | Removes the resolved threads the listing would show, with their replies, under the file's lock, and prints how many comments went. Takes `--branch <name>` and `--all-branches` like the listing. `--resolved` is required, so a bare `rm` removes nothing. It removes posted threads too, unlike the viewer's `clear_resolved` ([viewer.md](viewer.md#writing)), which leaves them because a review plugin's next update would bring them back. |

`comments` is a subcommand, so a revision named `comments` needs the explicit form: `dffy diff comments`.

Agents read the same comments through the MCP tool `list_comments` ([mcp.md](mcp.md#list_comments)), which can also say where each thread is now in a diff. `file_diff` and `diff` with `comments: true` return them beside their lines. To read them under their lines in a diff, print it with `--show-comments` ([usage.md](usage.md#review-comments-in-printed-output)).

### As a prompt: `dffy comments prompt`

`dffy comments prompt <id>` prints a thread as one prompt a coding agent can answer. `<id>` is any comment in the thread. The viewer's `copy_prompt` ([viewer.md](viewer.md#copying-a-thread)) copies the same prompt for a focused thread or selected lines, and MCP's `thread_prompt` ([mcp.md](mcp.md#thread_prompt)) returns it.

The prompt holds, in order:

1. Where the thread is: file, lines and side (`src/lib.rs:12-14 (the new version of the file)`), plus `resolved` or `outdated on its host` when it is.
   - The file's language, as dffy highlights it: `Language: rust`.
   - The definitions around all of the lines, outermost first, as the viewer's scope line names them: `In: mod tests > fn parse`. Only for lines placed on the diff, in a language with scopes ([viewer.md](viewer.md#the-scope-line)). The viewer leaves it out while it's still finding the file's definitions, and in a rendered view, rather than hold up the screen to find them.
2. Each comment under its author, as `octocat:`, or `you:` for yours, drafts included. Its text is indented by two spaces, and so is its suggestion, in a `suggestion` fenced block.
3. The commented lines, numbered and marked `>`, with up to 5 lines around them.
4. The file's nearby changes, as a unified diff in a `diff` fenced block.
5. The ask: answer the thread's last comment, and end with any change to the commented lines in a fenced block opened with ```` ```suggestion ````.

Every fence is longer than any run of backticks inside it. The prompt doesn't depend on the time or the viewer: the same thread and diff give the same prompt.

The thread is placed on what plain `dffy` shows: the uncommitted changes, or the unstaged ones with `default-diff = unstaged`. When that diff doesn't change the thread's file, or no line matches its anchor, the lines come from the anchor and the heading says `as written`. The viewer uses the diff on screen.

A prompt is at most 32 KiB. Over that, dffy cuts in this order until it fits:

1. the file's other hunks, farthest from the thread first
2. lines of the thread's hunks more than 5 lines from its lines
3. the lines around the commented lines, down to 3
4. replies between the first comment and the last two, earliest first, except those with a suggestion
5. the middle of the commented lines, keeping the first and last few

The location, language and definitions, the first comment, the last two comments, the suggestions and the ask are never cut, so a long enough thread makes a longer prompt. Each cut is marked: `[… 12 lines …]`, `[2 replies omitted]`, `[3 other hunks of this file omitted]`.

An `id` no comment has exits with status 128.

`dffy comments prompt --open` prints every open thread of yours on the current branch in one prompt: threads you started and threads you replied in. Each comes under its first comment's id (`## Thread 9f3c2a71b04e5d18`), as `dffy comments prompt <id>` gives it without its own ask. One ask for all follows: change only what each thread's last comment asks, and reply in each with what you changed. A line counts your open threads on other branches, which are left out. The viewer's `h` points an agent at this prompt ([viewer.md](viewer.md#fixing-with-an-agent)).

- The budget is 32 KiB for each of the first four threads, shared evenly. Up to four threads get 32 KiB each, eight get 16 KiB each. Each thread is cut alone, as above.
- `--json` prints one object: `{"threads":["9f3c2a71b04e5d18",…],"elsewhere":1,"prompt":"…","trimmed":false}`. `threads` is the ids in prompt order, `elsewhere` the count left out on other branches, and `trimmed` whether anything was cut.
- With no open thread of yours, it says so and exits 0.

### Who `you` is

In what dffy prints for an agent, only your comments are by `you`: those with no author.

- An author whose name starts with a word that reads as `you`, in any case, with invisible characters or common lookalike letters, shows as `someone named …`. A name is always one line.
- In a prompt, only dffy's own lines start at the margin. Comments are indented, and quoted code lines start with their mark, number or sign. Control characters and Unicode's line separators are shown by name (`^M`, `<U+2028>`), so no text passes for a `you:` line. Trimming keeps or drops a comment whole.
- The listing, the Markdown and MCP's text keep comments apart from their headings the same way. `--show-comments` draws them in boxes, where a comment's text can look like another's heading: agents read the prompt.

### What's handed over

A thread handed to an agent from the viewer ([viewer.md](viewer.md#fixing-with-an-agent)) goes as one line that says how to read it, never as its text. It starts `Address dffy review thread <id>:`, or `Address the open dffy review threads:` for all of them, and names the commands to run.

For a thread in the local review's file those are `dffy comments …`. For one in any other file they name it, `dffy --comments <file> comments …`, with an absolute path quoted for a shell. The file is named instead of `--session` because the [session pointer](#the-session-pointer) is whichever review any viewer of the repository opened last.

A reply belongs in the same file: `dffy --comments <file> comments apply --as <name>`.

## Listening: `dffy comments listen`

```sh
dffy comments listen --as claude --key $PPID
```

`dffy comments listen` is for a coding agent's session to run and read for as long as it lasts. It prints one line for each hand-over from a viewer in the working tree it runs in (or `-C`'s), [as above](#whats-handed-over). Viewers may open before or after it. [viewer.md](viewer.md#sending-to-a-running-session) has how the viewer picks among several sessions. Control characters in a line are escaped, so a hand-over is always one line.

| Option | Effect |
|---|---|
| `--as <name>` | The agent's name, which viewers show. Required, with the same rules as for `apply`. |
| `--key <key>` | The session's own name, which `dffy comments listening` reports by. Any string, such as the agent's process id. |

It ends when it's killed, when the reader of its stdout goes, or when the process that started it exits, so an agent's end doesn't leave it behind. Run it directly or with a shell's `exec`: a shell that stays between it and the agent may outlive the agent, and one that starts it with `&` and ends takes it along. On systems other than macOS and Linux (5.3 or later) it doesn't see the process that started it exit.

It waits out everything else, since an agent may act on its exit:

- Outside a working tree it stays and prints nothing.
- It starts dffyd when none runs ([watch.md](watch.md#dffyd)). When dffyd goes or can't be reached, it tries again, waiting longer each time, up to 30 seconds.
- With a dffyd too old to know it, it waits until that one is gone. `dffy daemon stop` replaces it.

What goes wrong is said on stderr, once each.

`dffy comments listening --key <key>` says that the session listening with that key was just prompted, or with `--edited` that it just edited a file. An agent's hooks run it. It starts nothing, prints nothing, not even what's wrong with the config, waits at most a second for each answer from dffyd, and exits 0 whether or not a session listens.

## Writing: `dffy comments apply`

```sh
echo '[{"op":"add","comment":{"path":"README.md","text":"Typo in the title."}}]' | dffy comments apply
```

`dffy comments apply` reads a JSON list of [operations](#operations) on stdin and applies them in one write. Stdin can't be a terminal.

It holds an advisory lock, `flock` on `<file>.lock` beside the file, waiting while another writer has it. Under the lock it reads the file as it is now, applies the operations in order, writes a temporary file in the same directory, and renames it over the file. Operations are all or nothing: when one fails, nothing is written. Put everything one change needs in one call, such as a page of imported threads, so other writers see it whole.

`--as <name>` sets the author of every `add` that has no `author` or an empty one. A coding agent passes its own name (`dffy comments apply --as claude`), so its replies are never yours. Without `--as`, they would be yours and, in a review session, [drafts](#drafts) that post under your name. The name is up to 64 characters, with no control characters, and can't be empty or read as [`you`](#who-you-is).

While a review is being posted, a `set` of a comment's `text` or `suggestion` is refused, since the post already read its drafts. The post holds `<file>.post` beside the file from reading the drafts until it ends. A batch with such a `set` holds it too while it writes.

### Operations

| Operation | Does |
|---|---|
| `{"op":"add","comment":{…}}` | Adds a comment. dffy assigns its `id`. |
| `{"op":"set","id":"…","fields":{…}}` | Changes some fields of the comment with that `id`. `"ref":"…"` instead of `"id"` picks the comment with that `ref`. |
| `{"op":"upsert","comment":{…}}` | For imports: updates the comment with the given `ref`, or adds it when none has it. |
| `{"op":"rm","id":"…"}` | Removes the comment with that `id` (or `"ref":"…"`), and its replies when it starts a thread. |
| `{"op":"gone","refs":["…"]}` | For imports: the host no longer has the comments with these refs. See [`gone`](#gone). |
| `{"op":"notice","notice":"…"}` | Sets the file's `notice`. `null` removes it. |
| `{"op":"summary","summary":"…"}` | Sets the file's `summary`. `null` removes it. |

#### `add`

The comment has the fields of [the file](#the-file-version-1), with these differences:

- It has no `id`.
- Fields that may be left out default to: `created` now, `author` you, `resolved` false, `side` `new`.
- A reply names its thread with `in_reply_to`, the `id` of any comment in the thread, or with `in_reply_to_ref`, its `ref`. That comment must already be in the file or added earlier in the same list. A reply needs only `text`. It takes its thread's `path`, `old_path` and `branch`, and any lines, anchor or branch given are dropped.

What each kind of comment needs:

| Kind | Needs |
|---|---|
| Line comment | `path`, `text`, `start`, `end`, and an `anchor` with at least one line. `start` needs a number on `start_side`, `end` on `side`. |
| File comment | `path` and `text`. |
| [General comment](#general-comments) | `ref` and `text`, and no lines. |

On the local review's file (no `--comments` or `--session`), a thread `add` starts without a `branch` field goes on the [current branch](#branches). `"branch":null` keeps it on no branch, so it shows on every branch. On any other file, such as an integration's, `add` doesn't fill in `branch`.

#### `set`

`fields` holds any of:

| Field | Sets |
|---|---|
| `text` | The text. Refused on a comment with a `ref`, unless the same `set` removes the `ref`: a posted comment's text is the host's. |
| `suggestion` | The suggestion. `null` removes it. Refused on a comment with a `ref`, as for `text`. Clears `applied` and `replaced`. |
| `author` | Only `""`. The comment becomes yours, as when you adopt an agent's comment, which makes it a [draft](#drafts) in a review session. Also clears `local_only`. Refused on a comment with a `ref`, as for `text`. |
| `resolved` | Whether the thread is resolved. Set it on the thread's first comment. |
| `ref` | The `ref`, after posting the comment. `null` removes it. A comment that has a `ref` takes no other. |
| `anchor` | The anchor, for a comment placed again on newer text. |
| `start`, `end` | A line comment's lines, given together, as in [the file](#the-file-version-1). For a thread moved to other lines, with its `anchor`. |
| `start_side` | The side of the first line. `null` removes it, for a range on `side` alone. |
| `applied` | When its suggestion was applied. `null` removes it, and `replaced` too unless the same `set` gives `replaced`. |
| `replaced` | The lines its suggestion replaced. `null` removes them. |
| `branch` | The thread's [branch](#branches), whichever of its comments the `set` names. It moves the whole thread, or with `null` puts it on no branch. |
| `pending` | Whether it's on the host but visible only to you. |
| `post_error` | Why the host refused it. `null` removes it. |

#### `upsert`

The comment needs a `ref` and has no `id`. It can name its thread with `in_reply_to` or `in_reply_to_ref`, as in `add`. A comment read from the file can be upserted back without its `id`.

- When a comment has that `ref`, the given fields replace its own, `text` included. Fields left out keep their values, and so do its `id` and fields dffy doesn't know. A field given as `null` is removed. `resolved` is ignored while the thread has a resolve or reopen not yet sent (its `host_resolved` differs from `resolved`), so an import doesn't undo it.
- Otherwise the comment is added, as by `add`.

### `gone`

An integration that read every comment on the host reports which of its earlier imports were deleted there. dffy removes them without losing anything of yours:

- A thread whose comments are all gone is removed whole.
- A reply is removed alone.
- A thread's first comment (or a general comment) that still has replies not gone, such as your drafts, stays as what they answer, but no longer posted. Its `ref`, `url`, `outdated`, `pending`, `host_resolved` and the fields dffy doesn't know are removed. If it's yours, it gets `deleted on the host` as its `post_error`, since it's a draft again. Its replies stay drafts. An integration can't post them in a thread the host no longer has.

A ref no comment has is ignored.

### The answer

On success, dffy prints one JSON object on stdout:

```json
{"ids":["e2a05d7c1f9b4836",null,"9f3c2a71b04e5d18"]}
```

`ids` has one entry per operation, in order: the `id` of the comment it added, changed or removed, or `null` for `gone`, `notice` and `summary`.

On failure, dffy prints the error on stderr, exits with status 128, and leaves the file unchanged. The message starts with the failing operation's position in the list, counting from 0:

```
error: operation 1 (set): comment 77c1e0a94b2d5f60 doesn't exist
```

An operation fails when:

- it's malformed: an unknown `op`, a field of the wrong type, or a comment missing what it needs;
- the comment it names doesn't exist;
- it would give a `ref` to a second comment, or another `ref` to a comment that has one;
- it sets `text`, `suggestion` or `author` on a comment with a `ref`, or `author` to anything but `""`;
- it removes a general comment's `ref`;
- it sets `start` without `end` or the other way round, or sets lines on a comment that isn't a line comment;
- the file is of a version dffy doesn't know, or isn't valid JSON.

## Importing: `dffy comments import`

```sh
dffy comments --session import ~/.local/share/dffy-review-github/github.com/acme/widgets/7/comments.json
```

`dffy comments import <file>` adds the comments of another comments file that this one doesn't have, drafts included, under this file's lock. It prints how many came: `imported 3 comments`.

- A comment is already here when this file has its `ref`, has its `id` on the same `path` and `created`, or has it as an earlier import's `moved_from`. It stays as this file has it.
- A comment whose `id` this file uses for another comment comes in under a new `id`, with the old one as its `moved_from`.
- A reply joins its thread however this file has the thread's first comment, wherever the reply sits in `<file>`.
- The `summary` comes too when this file has none.
- Importing the same file again adds nothing. `<file>` isn't changed. A file of a version dffy doesn't know is refused.

It's for comments kept outside dffy's files, such as those dffy-review-github kept for a PR before dffy kept them ([review-github.md](review-github.md#storage)). Such a file may keep an unsent resolve or reopen in a field of its own instead of `host_resolved`. That doesn't come with the import: resolve the thread again.

## Where the file is

| Comments | File |
|---|---|
| Your own changes | `dffy/comments.json` in the git directory every worktree of the repository shares (`.git/dffy/comments.json`, from a linked worktree too). |
| A code host's review, kept by a [review plugin](plugins.md#review-plugins) | `dffy/reviews/<plugin>/<id>.json` in the same shared git directory. `<plugin>` is the plugin's name and `<id>` its id for the review. Example: `.git/dffy/reviews/github/7.json` for dffy-review-github's PR #7. |
| Any file | `--comments <file>`, for every subcommand. A relative path is relative to the directory dffy runs in, which `-C` sets. |
| The review session the viewer has open | `dffy comments --session`, which reads the file the [session pointer](#the-session-pointer) names. |

- Outside a repository there's no default, and `dffy comments` fails until `--comments` names a file.
- In a review's file name, every byte except lowercase ASCII letters, digits, `-`, `_` and a `.` that isn't first is written `%XX`. Ids that differ only in case are then two files even where the file system ignores case, as on macOS. A name longer than a file name allows is cut to its first 150 bytes, then `-` and 16 hex digits of the id's SHA-1.
- Both files outlive the worktrees. Comments kept elsewhere, such as a plugin's own copy from before dffy kept reviews, come in with [`dffy comments import`](#importing-dffy-comments-import).
- A missing file reads as no comments. The first write creates it and its directory. The viewer creates the directory on start, to watch it.
- A diff piped to dffy has no comments. It has only hunks, not the file text a thread is placed on, so `--show-comments` reads the repository instead.

When the viewer has a code host's review open ([viewer.md](viewer.md#code-host-reviews)), it shows the review's file and also your drafts from the local file that place on the review's diff: your comments without a `ref`, with their replies. Edits to those drafts stay in the local file. So do new comments on a review that plain `dffy` found for the branch, until posted.

Posting (`l` in the post dialog) moves them into the review's file first. Each gets a new `id`, keeps its replies, loses its `branch`, and is removed from the local file once the review's file has it.

### Branches

One local file holds the comments of every branch and worktree. Each thread belongs to the branch it was written on, its `branch`. A comment belongs to the branch you were on when you wrote it, not to the commits on screen, so `dffy show <commit>`, `dffy main..HEAD` and every other diff use the current branch.

The current branch is HEAD's. While a rebase is stopped (on a conflict, or to edit a commit), HEAD is detached and the current branch is the one being rebased, as git records it in the worktree's `rebase-merge/head-name` or `rebase-apply/head-name`. On any other detached HEAD there's no current branch.

| Thread | Shows |
|---|---|
| On the current branch | Yes. |
| On no branch, such as one written on a detached HEAD or before comments had branches | On every branch. On a detached HEAD, only these show. |
| On another branch, including a renamed or deleted one | No. `dffy comments --all-branches` lists them. The viewer lists them last in its comments panel, folded to one row until `b` ([viewer.md](viewer.md#the-comments-panel)). |

The viewer gives a thread it starts in the local file the current branch. So does `dffy comments apply` for a thread [`add`](#add) starts without one. `set` with a new `branch` moves a thread, or with `null` puts it on no branch.

### Moving from the worktree's file

Before branches, each linked worktree kept its own file, `dffy/comments.json` in its own git directory (`.git/worktrees/<name>/dffy/comments.json`). The first time dffy opens the local file from such a worktree, it moves the old file to the shared place, under the shared file's lock, if the shared file doesn't exist yet. Once it exists, other worktrees' old files stay where they are. To keep their comments, apply them to the shared file with `dffy comments apply`. Moved comments have no `branch`, so they show on every branch.

### The session pointer

`dffy/session` in the shared git directory names the review session the viewer has open. `dffy comments --session` and agents ([mcp.md](mcp.md#list_comments)) use it to find the session's comments without asking the host. The viewer writes it when it opens or finds a review. It stays after the viewer exits, until another session replaces it.

```json
{"version":1,"comments":"/home/ada/src/widgets/.git/dffy/reviews/github/7.json","review":"7","branch":"feature/x","head":"e4f5a6b8c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6"}
```

| Field | Meaning |
|---|---|
| `version` | `1`. |
| `comments` | The session's comments file, as an absolute path. |
| `review` | The review plugin's id for the review. |
| `branch` | The [current branch](#branches) when it was opened. `null` on a detached HEAD. |
| `head` | The review's head commit, as a full id. |

Without a pointer, `dffy comments --session` fails and says so. A pointer that doesn't match this shape is an error, not an empty session.

### Drafts

A **draft** is a comment `P` posts. Only a review session's file, the one a review plugin keeps, has drafts. Elsewhere nothing is posted, so the viewer shows and counts no drafts. A comment is a draft when:

- it's yours (an empty `author`);
- it has no `ref` and no `local_only`;
- its thread's first comment isn't private.

**Private** comments show `private` in a review session. They stay out of the draft counts and of what `P` sends, and a review plugin's `post` leaves them alone. You can still edit or delete them. These comments are private:

| Comment | Why |
|---|---|
| Anyone else's without a `ref` | Posting it would put it out under your name. This covers an agent's answer the viewer writes ([viewer.md](viewer.md#asking-an-agent)), comments added with `dffy comments apply --as <name>` ([agents.md](agents.md)), a reply MCP's `reply_to_thread` writes (by the client's name), and a comment added through the [control API](control.md) (by the client's hello name: `dffy ctl` for `dffy ctl` and for clients that give none, `dffy mcp` for the MCP server's tools). |
| Yours with `local_only` | The viewer writes three kinds ([viewer.md](viewer.md#asking-an-agent)): a comment starting with `@<name>` that asks a local agent, which posted would mention that name on the host or trigger its bot; your question to an agent on selected lines, or your follow-up to its answer; and a note for Fix with agent ([viewer.md](viewer.md#fixing-with-an-agent)). |
| Your reply in a thread whose first comment is private | The thread isn't on the host and won't be. |

To make a private comment yours, `set` its `author` to `""` (`A` on its thread in the viewer). That clears `local_only` and makes it a draft.

When the host deletes a thread of yours that holds a draft reply, the thread stays for the reply ([`gone`](#gone)). Its first comment is a draft again, with `deleted on the host` as its `post_error`. Posting it would start a new thread, so the post dialog warns about it before it goes, as it does for any draft the host refused.

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
| `notice` | Optional. A line an integration sets for the viewer's status bar on reload, such as `comments as of 10:42, refresh failed`. |
| `summary` | Optional, in a review session's file: the review summary you're writing, before it's posted with the review. The viewer writes it when its post dialog closes, and removes it once a post that submits the review succeeds. |
| `comments` | The comments, in the order they were added. |

A comment:

| Field | Meaning |
|---|---|
| `id` | Assigned by dffy when the comment is added. It never changes and is never reused, even after the comment is removed. Treat it as opaque. |
| `path` | The file's path on the new side, relative to the repository root, as in [json.md](json.md#files). Left out for a [general comment](#general-comments). |
| `old_path` | The path on the old side, for a renamed file only. A comment on old-side lines shows on the file with this path when a diff lists it apart from `path`, as a diff that doesn't pair the rename does. |
| `side` | `old` or `new`: the side of the last commented line. |
| `start_side` | `old` or `new`: the side of the first commented line. Defaults to `side`. |
| `start`, `end` | The first and last commented lines, each `{"old": <n>, "new": <n>}`: the line's 1-based number on each side, or `null` on a side it isn't on, as for a line in [json.md](json.md#lines). A context line has both numbers, an added line only `new`, a removed line only `old`. Left out for a comment on the whole file. |
| `text` | The comment, as Markdown: CommonMark with GitHub's tables, strikethrough and task lists, which GitLab and Gerrit also read. The viewer renders it ([viewer.md](viewer.md#reading)), keeps line breaks, and hides HTML comments, so an integration passes a host's text through unchanged. A `suggestion` fenced block in the text shows as code. The viewer draws a suggested change from `suggestion`. |
| `suggestion` | Optional replacement text for the commented lines, without a final newline. Applying it reads CRLF as a line break and one final newline as the end of the last line, so text with either applies the same. Empty deletes the lines. |
| `author` | Who wrote it, a free string. Empty means you. Only your comments are [drafts](#drafts). |
| `created` | When it was written, RFC 3339 (`2026-11-17T10:42:00Z`). dffy writes UTC. Integrations give their host's timestamp. |
| `in_reply_to` | For a reply, the `id` of its thread's first comment. |
| `resolved` | Whether the thread is resolved. Read from the thread's first comment. |
| `ref` | Opaque, owned by an integration: the comment's identity on a host, such as dffy-review-github's `github:<node id>`. A comment with a `ref` has been posted. A `ref` is on one comment at most, and a comment keeps its `ref` until it's cleared. `set` refuses another, as two posts of one draft would cause. |
| `anchor` | The text of the commented lines, with up to 2 lines of context on each side. See [Anchors](#anchors). |
| `distance` | How many lines the commented lines are from the nearest change on their side, in the diff the comment was written on. 0 when they include one. Left out when unknown. See [Distance](#distance). |
| `url` | The comment on its host, set by an integration. `o` on a focused thread in the viewer opens it. Only `http` and `https` links open. |
| `outdated` | `true` when the host marks the thread outdated (its lines changed since). Read from the thread's first comment. Left out when false. The viewer shows it, and before you reply to one, shows the review plugin's warning about such threads if it has one. |
| `kind` | Optional, for a general comment: `description`, `review` or `comment`, set by the integration. See [General comments](#general-comments). |
| `label` | Optional words dffy shows as given. For a general comment, the integration's word for it, such as a review's verdict (`approved`). For an agent's reply, how the agent ran, as the agent plugin marks it (`read-only sandbox`), shown after its author. |
| `branch` | Optional, for a local comment: the [branch](#branches) it was written on. It belongs to the thread: read from the first comment, and taken by every reply. Left out for a thread that shows on every branch. |
| `pending` | `true` when the comment is on the host but visible only to you, as in a review not yet submitted. Set by an integration. Left out when false. |
| `post_error` | Optional: why the host refused the comment, set by an integration and cleared when a post succeeds. dffy sets `deleted on the host` on a thread of yours that [`gone`](#gone) keeps. `dffy comments` shows it under the comment. |
| `local_only` | `true` on a comment of yours that is never posted until you make it yours again, such as an `@<name>` ask, a question or follow-up to an agent, or a note for Fix with agent. See [Drafts](#drafts). Left out when false. |
| `host_resolved` | Optional, on a posted thread's first comment: whether the host has the thread resolved, as an integration last saw it. When `resolved` differs, it's a resolve or reopen not yet sent, which a review plugin's `post` sends. While they differ, an `upsert`'s `resolved` is ignored, so an import doesn't undo it. Left out for a thread the host can't resolve, or that no integration reported. |
| `moved_from` | Optional: the comment's `id` in the file it came from, when moving the local review's drafts to a review's file, or importing over an id this file uses, gave it a new one. A move or import that runs again after being cut short finds the comment by it and doesn't copy it twice. |
| `applied` | Optional: when the `suggestion` was applied to the working tree, RFC 3339. The viewer sets it ([viewer.md](viewer.md#applying-a-suggestion)), and then takes the suggestion back instead of applying it again. It's cleared when the `suggestion` changes, when the suggestion is taken back, and when the file no longer holds the suggested lines. It stays in this file: dffy never sends it to a review plugin, and the plugin's `post` leaves it alone. |
| `replaced` | Optional, with `applied`: the lines applying the `suggestion` replaced, each without its line ending, joined by `\n`. Taking the suggestion back restores them. Cleared with `applied`, and kept in this file like `applied`. |

- `id`, `text`, `author`, `created` and `resolved` are always present, and so is `path` on every comment except a general one. dffy leaves other fields out when they don't apply. A reader treats a `null` field as left out.
- A line comment has `start`, `end`, `side` and `anchor`. A file comment has none of them. A general comment has no `path` either. A reply has `in_reply_to` and its thread's `path` and `old_path`. Its place is its thread's, with no lines or anchor of its own.
- A line of `anchor` is the line without its `\n`. A preceding `\r` stays. In a line that isn't valid UTF-8, each invalid sequence is replaced by U+FFFD, and an anchor taken from it only matches the same replaced text.
- No field belongs to one host. The fields map to GitHub pull requests, GitLab merge requests, Gerrit changes, and reviews kept in the repository with no host. Host-specific data, such as a GitLab note's position or a Gerrit comment's character range, goes in the plugin's own fields ([Versions](#versions)).

### General comments

A general comment is on the review as a whole, not on a file: a pull request's description, a comment in its conversation, or the summary submitted with a review. Other hosts have the same, such as a merge request's notes or a change's messages.

```json
{"id":"5d0e7b13a9c2f486","text":"Looks good, one question inline.","author":"hubot","created":"2026-11-17T12:30:00Z","resolved":false,"ref":"github:PRR_kwDOA","url":"https://github.com/acme/widgets/pull/7#pullrequestreview-2001","kind":"review","label":"approved"}
```

- It has no `path`, `old_path`, `side`, lines or anchor.
- Only an integration adds one, importing it from a host, so it has a `ref`. `add` and `upsert` refuse a comment with no `path` and no `ref`, or with no `path` and lines. `set` won't remove its `ref`. dffy doesn't write general comments yet. One the host deleted while your replies hang off it stays, without its `ref` ([`gone`](#gone)).
- `kind` uses words that aren't any host's: `description` for the review's own description, `review` for the summary of a submitted review, `comment` for anything else. A reader shows an unknown kind as a comment.
- `label` is the integration's word for it, shown as given. dffy-review-github gives a review's state, such as `approved` or `changes requested`.
- A reader that doesn't know general comments reads one as a comment on no file, keeping `kind` and `label` as unknown fields.
- The viewer shows a review session's general comments as its [Conversation](viewer.md#conversation).

### Anchors

```json
{"before":["pub fn add(a: i32, b: i32) -> i32 {","    let x = a;"],"lines":["    let y = b;","    let sum = x + y;","    sum"],"after":["}",""]}
```

| Field | Meaning |
|---|---|
| `lines` | The commented lines, `start` to `end` on `side`. At least one. |
| `before`, `after` | Up to 2 lines before and after them on that side. Fewer at the start or end of the file. |

Line numbers go stale: a host moves a thread to a newer commit, and a local file changes under its comments. So dffy places a line comment by its anchor, against the text of its side:

1. **Exact.** The lines at `start` to `end` are the anchor's `lines`. The comment stays there.
2. **Moved.** The anchor's `before`, `lines` and `after`, in a row, appear exactly once within 1000 lines of `start`. The comment moves to that copy of `lines`.
3. **Unplaced.** Otherwise. The viewer shows the comment under its file's last row, headed with the lines `start` and `end` name on `side`, over the anchor's lines and context as they were. A commented line shows as unchanged when the comment numbers it on both sides. `i`'s list shows it separately, with its file. Printed output with `--show-comments` shows it the same way at the end of its file. `--json` and MCP report it as `lost` ([json.md](json.md#annotations)).

A range whose first line is on the other side (`start_side` isn't `side`, as when it runs from removed lines to added ones) is anchored and placed by its lines on `side`: from `start`'s number on `side` when it has one, else from `end` alone.

The viewer fills in anchors for the comments you make in it. An integration fills them in from git, using the file at the commit the host gives for each comment, not the diff on screen.

### Distance

`distance` counts lines on the comment's side, from the commented lines to the nearest change in the diff:

- A changed line on that side (added on `new`, removed on `old`) is as far as its number is from the nearest commented line, and 0 when it's one of them.
- A change on the other side sits between two lines of this side, and both are 1 from it. For example, lines removed next to a `new`-side comment.

The viewer writes it from the diff on screen, which may be your uncommitted changes rather than the review's. An integration that can diff what the host shows measures against that instead, and uses `distance` only when it can't. dffy-review-github posts a comment too far from a change as a file comment that quotes the lines.

### Versions

- A reader ignores fields it doesn't know, at the top level and in a comment, and dffy keeps them when it rewrites the file. Only those two levels: unknown fields inside `start`, `end` or `anchor` are dropped.
- An integration can store its own fields at those two levels. Prefix them with its name (`github_thread`) to avoid fields dffy may add. dffy hands a review plugin its fields back with the comment's `ref` ([plugins.md](plugins.md#comments)).
- New fields keep the version. Removing or renaming a field, or changing its type or meaning, bumps it.
- dffy refuses a file of a version it doesn't know, and never rewrites one.

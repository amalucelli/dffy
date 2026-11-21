# MCP server

`dffy mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server for coding agents such as Claude Code, Cursor and Zed's agent. It lets an agent ask which files a diff changes and read the diffs it needs, a page at a time. The results are the header and file objects of `dffy --json` ([json.md](json.md)), so a long change doesn't land in the agent's context all at once. It also lists the review comments left on the code ([comments.md](comments.md)), gives a thread as a prompt and takes a reply in it, and drives the viewer the user has open: it shows them a line or a thread, reads their selection and applies a thread's suggestion ([Viewer tools](#viewer-tools)).

An agent with a shell is better served by dffy's CLI, which the skill `dffy agents setup` installs teaches it to use ([agents.md](agents.md)). The MCP server is for hosts with no shell, such as Zed's and Cursor's agents without their terminal tool, and for sandboxes that keep `.git` read-only or block Unix sockets, as Codex's `workspace-write` mode may, where `dffy comments apply` and `dffy ctl` fail: the server runs outside the agent's sandbox.

It runs over stdio: the agent starts `dffy mcp` and talks to it on stdin and stdout. It serves the repository of the directory it starts in, or the one `-C <path>` names, and the comments file `--comments <file>` names, or the repository's.

## Registering it

**Claude Code**, from the repository:

```sh
claude mcp add dffy -- dffy mcp
```

**Cursor**, in `.cursor/mcp.json` at the project root (or `~/.cursor/mcp.json` for every project):

```json
{
  "mcpServers": {
    "dffy": { "command": "dffy", "args": ["mcp"] }
  }
}
```

**Zed**, in its settings file:

```json
{
  "context_servers": {
    "dffy": { "command": "dffy", "args": ["mcp"] }
  }
}
```

A client that starts servers outside the repository needs `-C`: `"args": ["-C", "/path/to/repo", "mcp"]`.

## Recommended agent prompt

The server tells agents to use its tools instead of `git diff`, but agents often reach for git in a shell anyway, especially when a request mentions commits. For an agent with a shell, the skill ([agents.md](agents.md)) does this. Otherwise, put this in the project's `CLAUDE.md`, `AGENTS.md` or equivalent:

```
To see what changed in the current git repository, use dffy tools instead of git diff, git show or git log -p.
```

In Claude Code, MCP tools can be deferred until the agent searches for them. That makes git in a shell the easier path, so this prompt matters there most.

## Tools

| Tool | What it returns | When to use it |
|---|---|---|
| `changed_files` | A page of file objects without hunks: path, status, counts, language. | First, to see what changed and how much. It's cheap even for thousands of files. |
| `file_diff` | One file object with its hunks and lines. | Next, for each file that matters. |
| `diff` | Several file objects with hunks, in path order. | For a small change, or everything under a few paths. |
| `list_comments` | Review comments, thread by thread. | To read what reviewers said about the code, or a local review's notes. |
| `thread_prompt` | One thread as a prompt to answer, with its lines and the file's changes near them. | To answer a thread, or propose a change to its lines. |
| `reply_to_thread` | The reply it wrote in a thread, under the client's name. | To say what you changed for a thread, or answer it, with a suggestion when you propose a change. |
| `open_in_viewer`, `focus_thread`, `viewer_selection`, `apply_suggestion` | What the viewer the user has open shows, or did. | To show the user a line or a thread, to read what they selected, or to apply a thread's suggestion. See [Viewer tools](#viewer-tools). |

With `syntax: true`, a file whose changes are only formatting gets `classification: "formatting_only"`, and one whose changes are only moved code gets `"moved_only"`. Its lines are marked `reformatted` or `moved`. An agent can skip such files, or read them knowing nothing was edited. [json.md](json.md#files) defines both.

Every diff tool takes `range`:

- **Default:** without `range`, a tool shows the uncommitted changes, `HEAD` against the working tree with untracked files included, as plain `dffy` does. With `default-diff = unstaged` in the config, it shows the unstaged changes instead.
- **Revisions:** `range` takes revisions as `git diff` does: `"main..HEAD"`, `"main...HEAD"`, `"HEAD~3"`, or `"v1.0 v2.0"`. Pass them as one string, split at spaces, or as an array.

`changed_files` and `file_diff` also take `since_checkpoint`, in place of `range`: `"latest"` compares the latest [checkpoint](git.md#checkpoints) with the working tree, and a number the checkpoint with that number, as `dffy --since-checkpoint` does: what an agent changed since the user handed it their review. Given with `range`, or with no such checkpoint, it's a tool error that says so.

Paths, in `paths` and `path`, are relative to the repository root, as the file objects give them. `paths` matches whole path components (`src` matches `src/lib.rs`, not `srcs/`) and takes no globs.

### `changed_files`

| Input | Default | Meaning |
|---|---|---|
| `range` | uncommitted changes | What to compare. |
| `since_checkpoint` | | `"latest"` or a checkpoint's number: what changed since it, in place of `range`. |
| `paths` | all | Only files under these paths. |
| `syntax` | `false` | Diff every file of the page by syntax, filling in `syntax` and `classification`. |
| `counts` | `true` | Count added and removed lines. `false` reads no file content at all. |
| `limit` | 200 | Files per page, 1 to 5000. |
| `cursor` | | The previous page's `next_cursor`. |

The result:

```json
{"header":{…},"files":[{"type":"file","path":"src/lib.rs",…,"hunks":null},…],"total_files":412,"counts":true,"next_cursor":"9b1f…"}
```

- **`files`:** summary file objects, as `dffy --json --stat` writes them ([json.md](json.md#summaries)).
- **`total_files`:** the number of files the whole diff changes.
- **`counts: false`:** for a very large range, `false` lists the files without reading them. Each file object then has `added` and `removed` null, `binary` and `too_large` false, and a language found from its name alone.

### `file_diff`

| Input | Default | Meaning |
|---|---|---|
| `path` | required | The file. A renamed file's old path finds it too. |
| `range` | uncommitted changes | What to compare. |
| `since_checkpoint` | | As for `changed_files`. |
| `syntax` | `false` | Diff by syntax: `moved` and `reformatted` marks, emphasis on changed tokens only, and the classification. |
| `context` | 3 | Unchanged lines around each change, 0 to 10000. |
| `max_lines` | 400 | Lines per result, across hunks, 1 to 20000. |
| `comments` | `false` | Add the file's review comment threads as `annotations`: the threads `list_comments` lists without arguments, each with the lines it's on in this diff, or `lost` ([json.md](json.md#annotations)). A file cut short has them all in its first part; the parts after have none. |
| `session` | `false` | With `comments`, the threads of the review session the viewer has open, as `list_comments` with `session` lists them. |
| `cursor` | | The previous result's `next_cursor`. |

The result:

```json
{"header":{…},"file":{"type":"file","path":"src/lib.rs",…,"hunks":[…]},"truncated":true,"remaining_lines":120,"next_cursor":"4c0e…"}
```

A path that names a directory, or a file the diff doesn't change, is a tool error that says so.

### `diff`

| Input | Default | Meaning |
|---|---|---|
| `range` | uncommitted changes | What to compare. |
| `paths` | all | Only files under these paths. |
| `syntax` | `false` | As for `file_diff`. |
| `context` | 3 | As for `file_diff`. |
| `max_files` | 20 | Files per page, 1 to 200. |
| `max_lines_per_file` | 100 | Lines per file, across its hunks, 1 to 20000. |
| `comments`, `session` | `false` | As for `file_diff`. |
| `cursor` | | The previous page's `next_cursor`. |

The result:

```json
{"header":{…},"files":[…],"truncated_files":[{"path":"big.c","remaining_lines":840,"cursor":"77aa…"}],"total_files":31,"next_cursor":"0d4e…"}
```

`truncated_files` lists the files of this page cut short. Each entry's `cursor` continues that file with `file_diff`, given the same `path`, `range`, `syntax`, `context`, `comments` and `session`.

### `list_comments`

Read-only: it reads the comments file on every call and never writes it. Writing is `dffy comments apply`'s job ([comments.md](comments.md#writing-dffy-comments-apply)), and [`reply_to_thread`](#reply_to_thread)'s for a reply.

| Input | Default | Meaning |
|---|---|---|
| `range` | | The diff each thread is placed on, for `placed`, as for the diff tools. Given, `placed` is filled. |
| `placed` | `false` | `true` fills `placed` on the uncommitted changes when `range` is left out. |
| `path` | all | Only the comments on this file, relative to the repository root. A renamed file's old path finds them too. |
| `open` | all | `true` for open threads only, `false` for resolved ones. |
| `resolved` | all | `true` for resolved threads only, `false` for open ones. Given with `open`, the two must agree. |
| `branch` | the current branch | The threads on this branch, and those on no branch ([comments.md](comments.md#branches)). |
| `all_branches` | `false` | Every branch's threads. Not with `branch`. |
| `session` | `false` | The comments of the review session the viewer has open, from the file the [session pointer](comments.md#the-session-pointer) names, instead of the local ones. Read as the review plugin last wrote it; the host isn't asked. An error when no session was opened. |
| `limit` | 100 | Comments per page, 1 to 1000. |
| `cursor` | | The previous page's `next_cursor`. |

Each filter judges a thread by its first comment, so a thread is in or out whole. Without `branch` or `all_branches`, it lists the current branch's threads and those on no branch, as `dffy comments` does; the current branch is read on every call. Outside a repository, with `--comments`, it lists every branch's.

The result:

```json
{"notice":null,"comments":[{"id":"9f3c2a71b04e5d18","path":"src/lib.rs","side":"new",…},…],"placed":{"9f3c2a71b04e5d18":{"side":"new","start":15,"end":17,"lost":false}},"standing":{},"total_comments":14,"next_cursor":null}
```

- **`standing`:** with `session`, where each comment on this page lives, by its `id`: `posted` on the host (it has a `ref`), `draft` for yours that posting sends, `local` for one kept here, which the viewer shows as `private`, such as an agent's answer or a question to an agent ([comments.md](comments.md#drafts)). Empty without `session`, as nothing in the local review's file is posted. Kept apart from `comments`, whose objects are as the file holds them.

- **`placed`:** where each line thread starting on this page is in `range`'s diff, by its first comment's `id`, as an annotation of `dffy --json --show-comments` gives it ([json.md](json.md#annotations)): the side and 1-based lines its anchor finds now, or `lost: true` with null lines when its lines are gone. A thread whose file the diff doesn't change, a comment on a whole file and a reply have no entry. It's filled only with `range`, or with `placed: true` on the uncommitted changes; otherwise it's empty and nothing is diffed. It costs a diff per file on the page with line threads: only the threads' files are listed and diffed. It's empty outside a repository, and with `placed: true` when the uncommitted changes can't be diffed, as without a worktree; a `range` given that can't be diffed is an error. It's kept apart from `comments`, whose objects are as the file holds them.
- **`comments`:** each comment as the file holds it ([comments.md](comments.md#the-file-version-1)), unknown fields included, in thread order: a thread's first comment, then its replies in the order they were added. [General comments](comments.md#general-comments), which have no `path`, come first; then threads by path and first line, file comments first. With `path`, general comments are left out. A page may end inside a thread; the next page continues it.
- **`notice`:** the file's `notice`, set by the integration that keeps the comments, such as that they're stale; null without one.
- **`total_comments`:** the number of comments the filters select, on every page.
- A missing comments file has no comments. Outside a repository, without `--comments`, the tool returns an error.
- In a linked worktree whose comments file hasn't [moved to the shared place](comments.md#moving-from-the-worktrees-file) yet, it reads the worktree's own file: it never moves or writes one.

### `thread_prompt`

Read-only, as `list_comments` is: one thread as the prompt `dffy comments prompt` prints ([comments.md](comments.md#as-a-prompt-dffy-comments-prompt)), placed on the uncommitted changes.

| Input | Default | Meaning |
|---|---|---|
| `id` | required | Any comment's id from `list_comments`, the thread's first or a reply. |
| `session` | `false` | The thread is in the review session the viewer has open, as `list_comments` with `session` lists it. |

The result:

```json
{"thread":"9f3c2a71b04e5d18","path":"src/lib.rs","prompt":"A review thread on src/lib.rs:12-14 (the new version of the file).\n\n…","trimmed":false}
```

- **`thread`:** the id of the thread's first comment.
- **`path`:** its file; null for a comment on the review as a whole.
- **`prompt`:** the prompt, at most 32 KiB unless the thread's own text is longer; also the text result.
- **`trimmed`:** something was cut to fit, each cut marked in `prompt`.

An id no comment has is a tool error.

### `reply_to_thread`

Writes a reply in a thread of the comments file, as `dffy comments apply` does ([comments.md](comments.md#writing-dffy-comments-apply)). An agent the viewer's Fix with agent started ([viewer.md](viewer.md#fixing-with-an-agent)) says here what it changed.

| Input | Default | Meaning |
|---|---|---|
| `id` | required | Any comment's id from `list_comments`, the thread's first or a reply. |
| `text` | required | The reply, as Markdown. |
| `suggestion` | | Lines to put in place of the thread's lines, without a final newline; empty removes them. |
| `session` | `false` | The thread is in the review session the viewer has open, as `list_comments` with `session` lists it. |

The result:

```json
{"id":"e2a05d7c1f9b4836","thread":"9f3c2a71b04e5d18","author":"claude-code"}
```

- **Author:** the client's name, as its `initialize` gives it in `clientInfo`, without control characters, up to 64 characters; `dffy mcp` when it gives none, as a client of the 2026-07-28 revision, which has no `initialize`, and when the name is `you`, as your own comments show. Never yours: on a review from a code host the reply shows `private`, and `P` leaves it until `A` on its thread makes it yours ([comments.md](comments.md#drafts)).
- **Never an ask:** a reply starting `@claude` asks no agent, as nothing but a comment typed in the viewer does.
- An id no comment has is a tool error that says so; nothing is written.

### Viewer tools

These drive the dffy viewer the user has open in the repository's working tree, through the [control API](control.md). When no viewer runs there, each returns a tool error that says so (`No dffy viewer is running in /path/to/repo: ask the user to open dffy there; …`); with several there, one that lists them. They never start a viewer.

| Tool | Input | The result |
|---|---|---|
| `open_in_viewer` | `path` (required), `line` (1-based), `side` (`new` or `old`, default `new`) | `{"view","path","line","side"}` once the file shows, at the line when given. A file not in the viewer's diff, or a line it doesn't have, is a tool error. |
| `focus_thread` | `id`, any comment's id from `list_comments` | `{"view","thread","path","focused"}`: the thread's first comment and its file; `focused` is false while its place is on its way, and it's focused once there. |
| `viewer_selection` | none | `{"view","selection"}`: `selection` is `{"path","side","start","end","text"}`, or null when nothing is selected. |
| `apply_suggestion` | `id`: a comment with a suggestion, or any of its thread's | `{"view","thread","comment","path","start","end","lines"}` once it's written, as `comments/applySuggestion` answers ([control.md](control.md)). |

The text says what happened (`The viewer shows src/lib.rs at line 42.`) or, for `viewer_selection`, gives the place and the selected lines.

#### `apply_suggestion`

Puts a suggestion in place of its thread's lines in the working tree, as `s` on the thread does in the viewer ([viewer.md](viewer.md#applying-a-suggestion)): the suggestion of the comment `id` names, or when it has none, of the last comment in its thread with one. The viewer writes it, against the diff it shows, so the result is an unstaged change the user sees at once and can discard. It's a tool error, writing nothing, when the lines changed since the suggestion was written, when the viewer's diff is neither the working tree's nor a review's or doesn't have the file loaded yet (`open_in_viewer` it first), on the old version's lines, or on someone else's review that isn't checked out. It never takes a suggestion back, as `s` on an applied one does: one applied already is a tool error (`this suggestion is applied already`). It's refused too while the viewer asks an agent: that agent may have this server among its MCP servers, and is to only read.

## Results

A successful call returns two things:

- **`structuredContent`:** the objects described above.
- **`content`:** the same result as text, for clients that show the model only text.

For the diff tools, the text is a unified diff with a status line per file, a second column for the syntax diff's marks (`>` moved, `~` reformatted), and the cursor to continue with when there is one:

```
M spacing.c +1 -1 [formatting_only]
@@ -1,3 +1,3 @@
  int x = 1;
-~int y = 2;
+~int  y=2;
  int z = 3;
M long.txt +12 -12
@@ -1,5 +1,2 @@
  line 1
  line 2
- line 3
- line 4
- line 5
[truncated: 30 more lines; next_cursor: … (pass it as cursor, with the same arguments)]
```

With `comments: true`, a file's text ends with its threads: each annotation's place (`a1 on new line 1:`, or `lines not found`), then its comments' authors, times and text.

For `list_comments`, the text is the listing of `dffy comments` ([comments.md](comments.md#reading-dffy-comments)) under a count of the comments, each thread in `placed` headed with where it is now (`src/lib.rs:12-14, now :15-17`, or `lines not found`). For `thread_prompt`, the text is the prompt.

## Paging and truncation

- **Pages of files:** `changed_files` and `diff` return files in the order printed output lists them (git's path order, renames paired). `next_cursor` is null on the last page.
- **Long files:** `file_diff` and `diff` cut a long file at a hunk boundary. A hunk too large for a whole result is cut at a line boundary. A hunk cut short gets `old_start`, `old_lines`, `new_start` and `new_lines` for the lines it holds, so every hunk in a result is consistent on its own. The parts of a file, read in order, hold each of its lines once.
- **Progress:** every result holds at least one line or one file, so following `next_cursor` always reaches the end.
- **Cursors:** a cursor is opaque. It continues only the request that returned it: the same tool and arguments, except `cursor`, `limit`, `max_files` and the line limits, which may change between pages.
- **Stale cursors:** a cursor is refused with a tool error when its request's arguments differ, or when the diff changed since. A diff changes when a revision now names another commit, a file joins or leaves the list, or, for `file_diff`, the file's content changes. For `list_comments`, the list changes when a comment is added or removed. Call again without `cursor` to start over.

## Limits

| Limit | Value | Over it |
|---|---|---|
| Line text per file in a result | 64KB past its first line | The file is cut there, as for `max_lines`. A single line longer than that is returned whole. |
| Line text per `diff` result | 64KB | The files after it move to the next page. |
| Files per page | `limit` (at most 5000), `max_files` (at most 200) | `next_cursor`. |

The limits of `--json` apply too: binary files, files of 64MB or more, and the syntax diff's size and time limits ([json.md](json.md#limits)).

## Settings

- **From the config file:** `renames`, `ignore-all-space`, `ignore-space-change` and `default-diff` apply as for `--json`, and the header records them.
- **From the tool inputs:** `context` and `syntax`. The config's `context` and `syntax-diff` don't apply.
- **Disk cache:** shared with every other dffy process, so syntax diffs and rename results computed once are reused.

## Protocol

- **Versions:** dffy serves protocol revision 2026-07-28, where every request carries its version in `_meta`, and the `initialize` handshake of revisions 2025-11-25 and 2025-06-18.
- **Negotiation:**
  - After `initialize`, requests are served as the revision it agreed on.
  - A request with a version dffy doesn't serve gets error -32022, with the versions it does serve.
- **Methods:** `server/discover`, `initialize`, `ping` (in the handshake revisions), `tools/list` and `tools/call`.
- **Errors:**
  - Bad tool arguments, unknown revisions and stale cursors are tool results with `isError: true` and a message the model can act on.
  - A tool name dffy doesn't have is protocol error -32602.
- **Requests:** answered one at a time, in order.
- **Shutdown:** the server exits when stdin closes.
- **Logging:** nothing but protocol messages goes to stdout.

# MCP server

`dffy mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server for coding agents such as Claude Code, Cursor and Zed's agent. Register it with `claude mcp add dffy -- dffy mcp`.

Its tools let an agent:

- list the files a diff changes, then read the diffs it needs a page at a time, as the header and file objects of `dffy --json` ([json.md](json.md));
- read the review comments on the code ([comments.md](comments.md)), get a thread as a prompt, and reply in it;
- drive the viewer the user has open: show a line or a thread, read the selection, apply a suggestion ([Viewer tools](#viewer-tools)).

An agent with a shell is better served by the CLI and the skill `dffy agents setup` installs ([agents.md](agents.md)). Use the MCP server for hosts with no shell, such as Zed's and Cursor's agents with their terminal tool off. Use it also in sandboxes that keep `.git` read-only or block Unix sockets, where `dffy comments apply` and `dffy ctl` fail. Codex's `workspace-write` mode may do this. The server runs outside the agent's sandbox.

The server speaks over stdio. It serves the repository of the directory it starts in, or the one `-C <path>` names. It uses the comments file `--comments <file>` names, or the repository's.

## Registering it

Claude Code, from the repository:

```sh
claude mcp add dffy -- dffy mcp
```

Cursor, in `.cursor/mcp.json` at the project root, or `~/.cursor/mcp.json` for every project:

```json
{
  "mcpServers": {
    "dffy": { "command": "dffy", "args": ["mcp"] }
  }
}
```

Zed, in its settings file:

```json
{
  "context_servers": {
    "dffy": { "command": "dffy", "args": ["mcp"] }
  }
}
```

A client that starts servers outside the repository needs `-C`: `"args": ["-C", "/path/to/repo", "mcp"]`.

## Recommended agent prompt

The server tells agents to use its tools instead of `git diff`. Agents still reach for git in a shell, especially when a request mentions commits. For an agent with a shell, the skill ([agents.md](agents.md)) handles this. Otherwise, put this line in the project's `CLAUDE.md`, `AGENTS.md` or equivalent:

```
To see what changed in the current git repository, use dffy tools instead of git diff, git show or git log -p.
```

It matters most in Claude Code, which can defer MCP tools until the agent searches for them. That makes git in a shell the easier path.

## Tools

| Tool | Returns | Use it |
|---|---|---|
| `changed_files` | A page of file objects without hunks: path, status, counts, language. | First, to see what changed and how much. Cheap even for thousands of files. |
| `file_diff` | One file object with its hunks and lines. | Next, for each file that matters. |
| `diff` | Several file objects with hunks, in path order. | For a small change, or everything under a few paths. |
| `list_comments` | Review comments, thread by thread. | To read what reviewers said, or a local review's notes. |
| `thread_prompt` | One thread as a prompt, with its lines and the file's changes near them. | To answer a thread or propose a change to its lines. |
| `reply_to_thread` | The reply it wrote, under the client's name. | To say what you changed for a thread, or answer it, optionally with a suggestion. |
| `definitions` | Where a name is defined at a version of the diff, in the languages [go to definition](viewer.md#go-to-definition) reads, and the lines using it. | To read the code a change calls, at either version, without a language server. |
| `open_in_viewer`, `focus_thread`, `viewer_selection`, `apply_suggestion` | What the user's viewer shows, or did. | To show the user a line or a thread, read their selection, or apply a suggestion ([Viewer tools](#viewer-tools)). |

These inputs work the same across the diff tools:

| Input | Meaning |
|---|---|
| `range` | Revisions, as `git diff` takes them: `"main..HEAD"`, `"main...HEAD"`, `"HEAD~3"`, `"v1.0 v2.0"`. One string split at spaces, or an array. Without it, the uncommitted changes: `HEAD` against the working tree, untracked files included, as plain `dffy` shows. With `default-diff = unstaged` in the config, the unstaged changes. |
| `since_checkpoint` | `changed_files` and `file_diff` only, instead of `range`. `"latest"` compares the latest [checkpoint](git.md#checkpoints) with the working tree, and a number compares that checkpoint, as `dffy --since-checkpoint` does. This is what an agent changed since the user handed over their review. It's a tool error together with `range`, or when the checkpoint doesn't exist. |
| `path`, `paths` | Relative to the repository root, as file objects give them. `paths` matches whole path components (`src` matches `src/lib.rs`, not `srcs/`) and takes no globs. |
| `syntax` | With `true`, a file whose changes are only formatting gets `classification: "formatting_only"`, and one whose changes are only moved code gets `"moved_only"`. Its lines are marked `reformatted` or `moved`. An agent can skip such files, or read them knowing nothing was edited ([json.md](json.md#files)). |

### `changed_files`

| Input | Default | Meaning |
|---|---|---|
| `range` | uncommitted changes | What to compare. |
| `since_checkpoint` | | `"latest"` or a checkpoint's number, instead of `range`. |
| `paths` | all | Only files under these paths. |
| `syntax` | `false` | Diff every file of the page by syntax, filling in `syntax` and `classification`. |
| `counts` | `true` | Count added and removed lines. `false` reads no file content at all. |
| `limit` | 200 | Files per page, 1 to 5000. |
| `cursor` | | The previous page's `next_cursor`. |

```json
{"header":{…},"files":[{"type":"file","path":"src/lib.rs",…,"hunks":null},…],"total_files":412,"counts":true,"next_cursor":"9b1f…"}
```

| Field | Meaning |
|---|---|
| `files` | Summary file objects, as `dffy --json --stat` writes them ([json.md](json.md#summaries)). |
| `total_files` | How many files the whole diff changes. |
| `counts` | As given. With `false`, for a very large range, each file object has `added` and `removed` null, `binary` and `too_large` false, and a language guessed from its name alone. |

### `file_diff`

| Input | Default | Meaning |
|---|---|---|
| `path` | required | The file. A renamed file's old path works too. |
| `range` | uncommitted changes | What to compare. |
| `since_checkpoint` | | As for `changed_files`. |
| `syntax` | `false` | Diff by syntax: `moved` and `reformatted` marks, emphasis on changed tokens only, and the classification. |
| `context` | 3 | Unchanged lines around each change, 0 to 10000. |
| `max_lines` | 400 | Lines per result, across hunks, 1 to 20000. |
| `comments` | `false` | Add the file's review threads as `annotations`: the threads `list_comments` lists without arguments, each placed on this diff or `lost` ([json.md](json.md#annotations)). A file cut into parts has them all in its first part. |
| `session` | `false` | With `comments`, the threads of the review session the viewer has open, as `list_comments` with `session` lists them. |
| `cursor` | | The previous result's `next_cursor`. |

```json
{"header":{…},"file":{"type":"file","path":"src/lib.rs",…,"hunks":[…]},"truncated":true,"remaining_lines":120,"next_cursor":"4c0e…"}
```

A path that names a directory, or a file the diff doesn't change, is a tool error.

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

```json
{"header":{…},"files":[…],"truncated_files":[{"path":"big.c","remaining_lines":840,"cursor":"77aa…"}],"total_files":31,"next_cursor":"0d4e…"}
```

`truncated_files` lists the files on this page that were cut short. Each entry's `cursor` continues that file with `file_diff`, given the same `path`, `range`, `syntax`, `context`, `comments` and `session`.

### `list_comments`

Reads the comments file on every call and never writes it. Use `dffy comments apply` ([comments.md](comments.md#writing-dffy-comments-apply)) or [`reply_to_thread`](#reply_to_thread) to write.

| Input | Default | Meaning |
|---|---|---|
| `range` | | The diff to place threads on, as for the diff tools. When given, `placed` is filled. |
| `placed` | `false` | `true` fills `placed` from the uncommitted changes when `range` is left out. |
| `path` | all | Only comments on this file, relative to the repository root. A renamed file's old path works too. |
| `open` | all | `true` for open threads only, `false` for resolved ones. |
| `resolved` | all | `true` for resolved threads only, `false` for open ones. Together with `open`, the two must agree. |
| `branch` | the current branch | The threads on this branch, and those on no branch ([comments.md](comments.md#branches)). |
| `all_branches` | `false` | Every branch's threads. Not with `branch`. |
| `session` | `false` | The review session the viewer has open, from the file the [session pointer](comments.md#the-session-pointer) names, instead of the local comments. It reads what the review plugin last wrote and doesn't ask the host. An error when no session was opened. |
| `limit` | 100 | Comments per page, 1 to 1000. |
| `cursor` | | The previous page's `next_cursor`. |

Filters judge a thread by its first comment, so a thread is in or out whole. Without `branch` or `all_branches`, it lists the current branch's threads and those on no branch, as `dffy comments` does. The current branch is read on every call. Outside a repository, with `--comments`, it lists every branch's.

```json
{"notice":null,"comments":[{"id":"9f3c2a71b04e5d18","path":"src/lib.rs","side":"new",…},…],"placed":{"9f3c2a71b04e5d18":{"side":"new","start":15,"end":17,"lost":false}},"standing":{},"total_comments":14,"next_cursor":null}
```

| Field | Meaning |
|---|---|
| `comments` | Each comment as the file holds it ([comments.md](comments.md#the-file-version-1)), unknown fields included. [General comments](comments.md#general-comments) come first, then threads by path and first line, file comments first. Within a thread, the first comment comes first, then replies in the order they were added. With `path`, general comments are left out. A page may end inside a thread, and the next page continues it. |
| `placed` | Where each line thread starting on this page is in `range`'s diff, keyed by its first comment's `id`. Each entry is `{"side","start","end","lost"}`, as an annotation of `dffy --json --show-comments` gives it ([json.md](json.md#annotations)). `lost: true` has null lines. Threads whose file the diff doesn't change, file comments and replies have no entry. |
| `standing` | With `session`, where each comment on this page lives, keyed by `id`: `posted` on the host (it has a `ref`), `draft` for yours that posting sends, or `local`, kept here and shown as `private`, such as an agent's answer or a question to an agent ([comments.md](comments.md#drafts)). Empty without `session`. |
| `notice` | The file's `notice`, such as that the comments are stale. Null without one. |
| `total_comments` | How many comments the filters select, on every page. |
| `next_cursor` | The next page's cursor, or null. |

`placed` is filled only with `range`, or with `placed: true`. Otherwise it's empty and nothing is diffed. Filling it costs one diff per file on the page that has line threads. It's also empty outside a repository, and with `placed: true` when the uncommitted changes can't be diffed, as without a worktree. A `range` that can't be diffed is an error.

A missing comments file has no comments. Outside a repository, without `--comments`, the tool returns an error. In a linked worktree whose comments file hasn't [moved to the shared place](comments.md#moving-from-the-worktrees-file) yet, it reads the worktree's own file and never moves it.

### `thread_prompt`

Returns one thread as the prompt `dffy comments prompt` prints ([comments.md](comments.md#as-a-prompt-dffy-comments-prompt)), placed on the uncommitted changes. Read-only.

| Input | Default | Meaning |
|---|---|---|
| `id` | required | Any comment's id from `list_comments`, the thread's first or a reply. |
| `session` | `false` | The thread is in the review session the viewer has open. |

```json
{"thread":"9f3c2a71b04e5d18","path":"src/lib.rs","prompt":"A review thread on src/lib.rs:12-14 (the new version of the file).\nLanguage: rust\nIn: fn add\n\n…","trimmed":false}
```

| Field | Meaning |
|---|---|
| `thread` | The id of the thread's first comment. |
| `path` | Its file. Null for a comment on the review as a whole. |
| `prompt` | The prompt, also returned as the text result. At most 32 KiB, unless the thread's own text is longer. |
| `trimmed` | Something was cut to fit. Each cut is marked in `prompt`. |

An unknown id is a tool error.

### `reply_to_thread`

Writes a reply in a thread, as `dffy comments apply` does ([comments.md](comments.md#writing-dffy-comments-apply)). An agent started by the viewer's Fix with agent ([viewer.md](viewer.md#fixing-with-an-agent)) uses it to say what it changed.

| Input | Default | Meaning |
|---|---|---|
| `id` | required | Any comment's id from `list_comments`, the thread's first or a reply. |
| `text` | required | The reply, as Markdown. |
| `suggestion` | | Lines to replace the thread's lines, without a final newline. Empty deletes them. |
| `session` | `false` | The thread is in the review session the viewer has open. |

```json
{"id":"e2a05d7c1f9b4836","thread":"9f3c2a71b04e5d18","author":"claude-code"}
```

- The author is the client's name from `clientInfo` in `initialize`, without control characters, up to 64 characters. It's `dffy mcp` when the client gives none, which includes every 2026-07-28 client since that revision has no `initialize`, and when the name reads as `you` ([comments.md](comments.md#who-you-is)).
- The reply is never yours. On a code host's review it shows `private`, and `P` skips it until `A` on its thread makes it yours ([comments.md](comments.md#drafts)).
- A reply starting with `@claude` asks no agent. Only a comment typed in the viewer does.
- An unknown id is a tool error, and nothing is written.

### `definitions`

Finds where a name is defined, as the viewer's go to definition does ([viewer.md](viewer.md#go-to-definition)), through the same lookup in dffy-core. It needs no viewer. Names are matched, then resolved where the asking file's imports say what they mean. A name defined in several places has several candidates, nearest the asking file first. Read-only.

| Input | Default | Meaning |
|---|---|---|
| `name` | required | The name: letters, digits and `_`. |
| `path` | | The asking file, relative to the repository root, in a language go to definition reads ([viewer.md](viewer.md#go-to-definition) lists them). Its own definitions rank first, then what it includes or imports, then its folder or crate. Without it, every definition of the name counts, by path, the first 200 listed. Another language's file is a tool error, and so is a version with no file of those languages. |
| `line`, `column` | | Where the name is used in `path`, 1-based, in the version looked in. `column` is a byte of the line. Without it, the name's first use in code on the line counts. They need `path`, and make the answer much better ([below](#giving-the-place)). |
| `side` | `new` | Which version of the diff to look in: `new` or `old`. |
| `range`, `since_checkpoint` | uncommitted changes | The diff, as for `file_diff`. Without them, `new` is the working tree and `old` is `HEAD`. |
| `references` | `false` | Add the lines using the name. |
| `calls_only` | `false` | With `references`, only the lines calling the name: followed by its arguments, past a Rust macro's `!` or type arguments. |
| `in_diff` | `false` | With `references`, only the lines in files this diff changes. |
| `limit` | 100 | Lines using the name per page, 1 to 1000. |
| `cursor` | | The previous page's `next_cursor`, which continues `references`. |

```json
{"name":"kmalloc_array","side":"new","candidates":[{"path":"include/linux/slab.h","start":612,"end":621,"kind":"function","container":null,"changed":true,"in_hunk":false,"text_match":false,"text":"static inline void *kmalloc_array(size_t n, size_t size, gfp_t flags)"}],"total":4,"progress":{"indexed":55554,"files":55554},"partial":false,"complete":true,"references":null,"next_cursor":null}
```

| Field | Meaning |
|---|---|
| `candidates` | The definitions found, best first, at most 200 ([below](#candidates)). |
| `total` | Definitions found, beyond those listed. |
| `progress`, `partial` | How far the version's index of definitions is built. While `partial` is true, a definition may be missing: the answer comes from the files built so far and from the asking file's own includes and uses, read on the spot. Ask again for the full answer. |
| `text_match` | No definition was found, so `candidates` are up to 50 lines using the name as a whole word, `kind` null, closest files first. A call searches for about 2 seconds. The next call with the same arguments goes on with the same search. |
| `complete` | False while asking again could list more: the index is `partial`, or the text search hasn't been through every file nor found its 50 lines. |
| `references` | With `references`, a page of the lines using the name ([below](#references)). Null without it. |
| `note` | Only there when the helper couldn't keep the index in the disk cache and keeps it in memory: why. |

Two paths compared outside git, and a disk cache turned off (`cache = false`, `--no-cache`), are tool errors: the index lives in the cache.

#### Giving the place

With `line`, what the use itself names comes first: a local variable, a parameter or the import that brings the name in. An import under another name finds what it names.

#### Candidates

| Field | Meaning |
|---|---|
| `start`, `end` | The definition's first and last lines, 1-based. |
| `kind` | `function`, `method`, `macro`, `type`, `struct`, `union`, `enum`, `enumerator`, `trait`, `constant`, `static`, `variable` or `module`. With `line`, also `local`, `parameter` or `import` for what the asking file itself defines the name as. |
| `container` | The type a method or an item of an impl or trait is on. |
| `changed` | The diff changes the file. |
| `in_hunk` | The diff adds or removes lines inside the definition. Null where that wasn't worked out (past the first few candidates). |
| `text` | The first line. |

#### References

`references` is `{"matches","total","files","complete"}`. `matches` is a page of the lines using the name as a whole word, each `{"path","line","text","changed","more"}`, closest files first. They come from the first 200 files with one and the first 1000 lines of each. `more` counts a file's lines past those, on its last one listed.

- In files of the languages go to definition reads, a use in a comment or string doesn't count, a block comment over several lines included. Nor does the line inside each definition of the name that names it, once the index has its file.
- A call searches for about 2 seconds, until it has a line past the page, or until it has found the 200 files. The next call's cursor goes on with the same search.
- `total` and `files` count the lines and the files found so far.
- `complete` is true once every file was searched with the index whole. Past the 200 files the rest isn't searched, so `complete` stays false and `files` is a lower bound.

#### The helper

The first call starts the navigator helper ([navigate.md](navigate.md)), which builds the index on 2 threads at a nice of 10 and keeps it in the disk cache for later runs. A helper that exits is started again on the next call, at most 3 times a minute. One that can't start, or keeps exiting, is a tool error, `definitions unavailable: …`.

The helper keeps the navigators of the two versions asked about last, and builds only the index of the one used last. Another's build goes on once it's asked about again. A version is read again when the core says what it answers from moved: another revision, a file of the diff edited again, the index staged to.

After 10 minutes without a `definitions` call, the server closes the helper, which gives its memory back, as the next call of any tool starts. The next `definitions` starts another.

### Viewer tools

These drive the dffy viewer the user has open in the repository's working tree, through the [control API](control.md). They never start a viewer. With none running there, each returns a tool error (`No dffy viewer is running in /path/to/repo: ask the user to open dffy there; …`). With several, the error lists them.

| Tool | Input | Result |
|---|---|---|
| `open_in_viewer` | `path` (required), `line` (1-based), `side` (`new` or `old`, default `new`) | `{"view","path","line","side","listed"?}` once the file shows, at the line when given. A file the viewer's diff doesn't change opens whole at `side`, and the result then has `"listed":false` ([control.md](control.md#files-outside-the-diff)). No file at that path and side, a line the file doesn't have, or a viewer still listing the diff's files (ask again) is a tool error. |
| `focus_thread` | `id`, any comment's id from `list_comments` | `{"view","thread","path","focused"}`. `thread` is the first comment's id. `focused` is false until the thread's place in the diff has loaded. |
| `viewer_selection` | none | `{"view","selection"}`. `selection` is `{"path","side","start","end","text"}`, or null when nothing is selected. |
| `apply_suggestion` | `id`, a comment with a suggestion or any comment in its thread | `{"view","thread","comment","path","start","end","lines"}` once written, as `comments/applySuggestion` answers ([control.md](control.md)). |

The text result says what happened (`The viewer shows src/lib.rs at line 42.`), and for a file outside the diff adds `It isn't in the diff, so it shows whole at the new side.` For `viewer_selection`, it gives the place and the selected lines.

#### `apply_suggestion`

Replaces a thread's lines in the working tree with a suggestion, as `s` on the thread does in the viewer ([viewer.md](viewer.md#applying-a-suggestion)). It uses the suggestion of the comment `id` names, or else the last comment in the thread that has one. The viewer writes it against the diff it shows, so the result is an unstaged change the user sees at once and can discard.

It's a tool error, and nothing is written, when:

- the lines changed since the suggestion was written;
- the viewer's diff is neither the working tree's nor a review's;
- the file's diff isn't loaded yet (call `open_in_viewer` first);
- the thread is on the old version's lines;
- the review is someone else's and isn't checked out;
- the suggestion is already applied (`this suggestion is applied already`). Unlike `s`, it never takes one back;
- the viewer is asking an agent. That agent may have this server among its MCP servers, and it's meant to only read.

## Results

A successful call returns:

- `structuredContent`: the objects described above;
- `content`: the same result as text, for clients that show the model only text.

For the diff tools, the text is a unified diff. Each file has a status line, a second column carries the syntax diff's marks (`>` moved, `~` reformatted), and the cursor to continue with comes last:

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

With `comments: true`, a file's text ends with its threads: each annotation's place (`a1 on new line 1:`, or `lines not found`), then each comment's author, time and text.

For `list_comments`, the text is the listing of `dffy comments` ([comments.md](comments.md#reading-dffy-comments)) under a count of the comments. Each thread in `placed` is headed with where it is now (`src/lib.rs:12-14, now :15-17`, or `lines not found`). For `thread_prompt`, the text is the prompt.

## Paging and truncation

- `changed_files` and `diff` return files in the order printed output lists them (git's path order, renames paired). `next_cursor` is null on the last page.
- `file_diff` and `diff` cut a long file at a hunk boundary. A hunk too large for one result is cut at a line boundary. A cut hunk gets `old_start`, `old_lines`, `new_start` and `new_lines` for the lines it holds, so every hunk is consistent on its own. Read in order, a file's parts hold each line once.
- Every result holds at least one line or one file, so following `next_cursor` always reaches the end.
- A cursor is opaque. It continues only the request that returned it: the same tool and arguments, except `cursor`, `limit`, `max_files` and the line limits, which may change between pages.
- A cursor is refused with a tool error when its request's arguments differ, or when the diff changed since. The diff changes when a revision now names another commit, a file joins or leaves the list, or, for `file_diff`, the file's content changes. For `list_comments`, the list changes when a comment is added or removed. Call again without `cursor` to start over.

## Limits

| Limit | Value | Over it |
|---|---|---|
| Line text per file in a result | 64KB past its first line | The file is cut there, as for `max_lines`. A single line longer than that is returned whole. |
| Line text per `diff` result | 64KB | The remaining files move to the next page. |
| Files per page | `limit` (at most 5000), `max_files` (at most 200) | `next_cursor`. |

The limits of `--json` apply too: binary files, files of 64MB or more, and the syntax diff's size and time limits ([json.md](json.md#limits)).

## Settings

- The config file's `renames`, `ignore-all-space`, `ignore-space-change` and `default-diff` apply as for `--json`, and the header records them.
- `context` and `syntax` come from the tool inputs. The config's `context` and `syntax-diff` don't apply.
- The disk cache is shared with every other dffy process, so syntax diffs and rename results are computed once.

## Protocol

| Topic | Behavior |
|---|---|
| Versions | Revision 2026-07-28, where every request carries its version in `_meta`, and the `initialize` handshake of revisions 2025-11-25 and 2025-06-18. After `initialize`, requests are served as the revision it agreed on. |
| Unsupported version | Error -32022, listing the versions dffy serves. |
| Methods | `server/discover`, `initialize`, `ping` (handshake revisions only), `tools/list`, `tools/call`. |
| Tool errors | Bad arguments, unknown revisions and stale cursors are tool results with `isError: true` and a message the model can act on. |
| Unknown tool | Protocol error -32602. |
| Ordering | Requests are answered one at a time, in order. |
| Shutdown | The server exits when stdin closes. |
| Logging | Only protocol messages go to stdout. |

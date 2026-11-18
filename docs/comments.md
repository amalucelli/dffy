# Review comments

**Experimental.** The file format and the operations may change.

dffy keeps review comments in a JSON file: comments on lines of a diff, replies to them, and whether a thread is resolved. The viewer draws them, and integrations such as `dffy-review-github` keep the file in step with a code host.

- dffy reads the file whenever it changes, so a comment written by another program shows up in a running viewer.
- Only dffy writes it: the viewer, and `dffy comments apply` for everyone else. An integration sends [operations](#writing-dffy-comments-apply) and dffy applies them under a lock, so two writers never lose each other's changes.
- dffy never talks to the network. Posting comments to a host and pulling them back is the integration's job.

## Where the file is

- `--comments <file>` picks the file, for every subcommand. An integration keeps one per pull request in its own storage and passes it when it starts dffy. A relative path is relative to the directory dffy runs in, which `-C` sets.
- Without it, the file is `dffy/comments.json` in the repository's git directory (`.git/dffy/comments.json`, or the worktree's own git directory in a linked worktree), for reviewing your own changes. Outside a repository there's no default, and `dffy comments` fails until `--comments` names a file.
- A file that doesn't exist reads as no comments. The first write creates it, and its directory.
- A diff piped to dffy has no comments.

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
| `comments` | The comments, in the order they were added. |

A comment:

| Field | Meaning |
|---|---|
| `id` | A string dffy assigns when the comment is added. It never changes and is never given to another comment, even after this one is removed. Treat it as opaque. |
| `path` | The file's path on the new side, relative to the repository root, as in [json.md](json.md#files). |
| `old_path` | The path on the old side, for a renamed file only. |
| `side` | `old` or `new`: the side of the last commented line. |
| `start_side` | `old` or `new`: the side of the first commented line. When it's left out, it's `side`. |
| `start`, `end` | The first and last commented lines, each `{"old": <n>, "new": <n>}`: the line's number on each side, 1-based, or `null` on a side it isn't on, as a line in [json.md](json.md#lines). A context line has both numbers, an added line only `new`, a removed line only `old`. Left out for a comment on the whole file. |
| `text` | The comment, as Markdown. |
| `suggestion` | Optional replacement text for the commented lines, without a final newline. |
| `author` | Who wrote it, a free string. Empty means you. |
| `created` | When it was written, RFC 3339 (`2026-11-17T10:42:00Z`). dffy writes UTC; integrations give their host's timestamp. |
| `in_reply_to` | For a reply, the `id` of its thread's first comment. |
| `resolved` | Whether the thread is resolved. It's read from the thread's first comment. |
| `ref` | Opaque, owned by an integration: the comment's identity on a host, such as `github:<node id>`. A comment with a `ref` has been posted. Each `ref` is on one comment at most. |
| `anchor` | The text of the commented lines, with up to 2 lines of context on each side. See [Anchors](#anchors). |
| `distance` | How many lines the commented lines are from the nearest change on their side, 0 when they include one. Hosts accept comments only within their hunks' context (3 lines on GitHub), so an integration compares it with that. Left out when it isn't known. See [Distance](#distance). |
| `url` | The comment on its host, set by an integration. `o` on a focused thread in the viewer opens it; only `http` and `https` links open. |
| `outdated` | `true` when the host marks the thread outdated (its lines changed since). Read from the thread's first comment; left out when false. The viewer shows it, and warns that GitHub collapses such threads before you reply to one. |

- `id`, `path`, `text`, `author`, `created` and `resolved` are always present. dffy leaves out the other fields when they don't apply; a reader treats a field that's `null` as left out.
- **Kinds of comment:**
  - A **line comment** has `start`, `end`, `side` and `anchor`.
  - A **file comment** has none of them.
  - A **reply** has `in_reply_to`, and its thread's `path` and `old_path`; its place is its thread's. It has no lines or anchor of its own.
- **Text:** a line of `anchor` is the line without its `\n`; a `\r` before it stays. A line that isn't valid UTF-8 has each invalid sequence replaced by U+FFFD, and an anchor taken from it only matches the same replaced text.

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
3. **Unplaced:** otherwise. The viewer lists the comment apart from the diff, with its file.

A range whose first line is on the other side (`start_side` isn't `side`, as when it runs from removed lines to added ones) is anchored and placed by its lines on `side`: from `start`'s number on `side` when it has one, else `end` alone.

The viewer fills in anchors for the comments you make in it. An integration fills them in from git, from the file at the commit the host gives for each comment, not from the diff on screen.

### Distance

`distance` counts lines on the comment's side from the commented lines to the nearest change in the diff:

- A changed line on that side (added on `new`, removed on `old`) is as far as its number is from the nearest commented line, and 0 when it's one of them.
- A change on the other side sits between two lines of this side. Both of those lines are 1 from it, as when lines were only removed and the comment is on the `new` side next to where they were.

The viewer writes it when you add a comment, from the diff on screen. An integration posting a comment with a `distance` over its host's context posts it as a file comment instead, quoting the lines.

### Versions

- A reader ignores fields it doesn't know, at the top level and in a comment, and dffy keeps them when it rewrites the file. Only those two levels: unknown fields inside `start`, `end` or `anchor` are dropped. An integration can store its own fields there; prefix them with its name (`github_thread`) to keep clear of fields dffy may add.
- New fields keep the version. Removing or renaming a field, or changing its type or meaning, bumps it.
- dffy refuses a file of a version it doesn't know, and never rewrites one.

## Reading: `dffy comments`

```sh
dffy comments                     # the threads, for reading
dffy comments --json              # the file, for scripts
dffy --comments pr.json comments  # another file
```

`dffy comments` lists the comments as threads. Each thread starts with its file and lines (`src/lib.rs:12-14`, `(old side)` for lines on the old side, nothing more for a file comment) and `resolved` when it is. Then come its comments in the order they were added, each with its `id`, author (`you` when it's empty), time and `posted` when it has a `ref`, over its text. Threads go by path, then by first line. The file's `notice`, when it has one, comes first.

For [the file above](#the-file-version-1):

```
notice: comments as of 10:42, refresh failed

src/lib.rs:12-14
  9f3c2a71b04e5d18 you, 2026-11-17T10:42:00Z
    This can overflow.
  1b7e94c0d25a6f03 octocat, 2026-11-17T11:03:12Z, posted
    Fixed, thanks.
```

`dffy comments --json` prints the whole file in [its format](#the-file-version-1), as dffy would write it, and never colored or paged. A file that doesn't exist prints as an empty one, `{"version":1,"comments":[]}` (with a line break in the list). Scripts read the file this way rather than opening it, so they never see a version they don't know: dffy refuses it and exits with status 128.

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

**`add`.** The comment has the fields of [the file](#the-file-version-1), except:

- no `id`;
- `created` may be left out for now;
- `author` may be left out for you, and `resolved` for false;
- `side` may be left out for `new`;
- a reply names its thread with `in_reply_to`, the `id` of any comment in the thread, or with `in_reply_to_ref`, its `ref`. That comment must be in the file already or added earlier in the same list. A reply needs only `text`; it takes its thread's `path` and `old_path`, and any lines or anchor it's given are dropped.

A line comment needs `start`, `end` and an `anchor` with at least one line; `start` needs a number on `start_side`, and `end` on `side`. A file comment needs `path` and `text`.

**`set`.** `fields` holds any of:

| Field | Sets |
|---|---|
| `text` | The text. Refused on a comment with a `ref`: it's posted, and its text is the host's. |
| `resolved` | Whether the thread is resolved; set it on the thread's first comment. |
| `ref` | The `ref`, after posting the comment; `null` removes it. |
| `anchor` | The anchor, for a comment placed again on newer text. |

**`upsert`.** The comment needs a `ref` and has no `id`, and can name its thread with `in_reply_to` or `in_reply_to_ref` as in `add`. A comment read from the file is upserted back without its `id`.

- When a comment has that `ref`, the fields given replace its own, `text` included; the fields left out keep their values, and so do its `id` and the fields dffy doesn't know. A field given as `null` is removed.
- Otherwise it's added, as by `add`.

### The answer

On success, dffy prints one JSON object on stdout:

```json
{"ids":["e2a05d7c1f9b4836",null,"9f3c2a71b04e5d18"]}
```

`ids` has an entry per operation, in order: the `id` of the comment it added, changed or removed, or `null` for `notice`.

On failure, dffy prints the error on stderr and exits with status 128, and the file is unchanged. The message starts with the failing operation's position in the list, from 0:

```
error: operation 1 (set): comment 77c1e0a94b2d5f60 doesn't exist
```

An operation fails when:

- it's malformed: an unknown `op`, a field of the wrong type, or a comment missing what it needs;
- the comment it names doesn't exist;
- it would give a `ref` to a second comment;
- it sets `text` on a comment with a `ref`;
- the file is of a version dffy doesn't know, or isn't valid JSON.

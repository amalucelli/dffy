# MCP server

`dffy mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server for coding agents such as Claude Code, Cursor and Zed's agent. It lets an agent ask which files a diff changes and read the diffs it needs, a page at a time. The results are the header and file objects of `dffy --json` ([json.md](json.md)), so a long change doesn't land in the agent's context all at once.

It runs over stdio: the agent starts `dffy mcp` and talks to it on stdin and stdout. It serves the repository of the directory it starts in, or the one `-C <path>` names.

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

## Tools

| Tool | What it returns | When to use it |
|---|---|---|
| `changed_files` | A page of file objects without hunks: path, status, counts, language. | First, to see what changed and how much. It's cheap even for thousands of files. |
| `file_diff` | One file object with its hunks and lines. | Next, for each file that matters. |
| `diff` | Several file objects with hunks, in path order. | For a small change, or everything under a few paths. |

With `syntax: true`, a file whose changes are only formatting gets `classification: "formatting_only"`, and one whose changes are only moved code gets `"moved_only"`. Its lines are marked `reformatted` or `moved`. An agent can skip such files, or read them knowing nothing was edited. [json.md](json.md#files) defines both.

Every tool takes `range`:

- **Default:** without `range`, a tool shows the uncommitted changes, `HEAD` against the working tree with untracked files included, as plain `dffy` does. With `default-diff = unstaged` in the config, it shows the unstaged changes instead.
- **Revisions:** `range` takes revisions as `git diff` does: `"main..HEAD"`, `"main...HEAD"`, `"HEAD~3"`, or `"v1.0 v2.0"`. Pass them as one string, split at spaces, or as an array.

Paths, in `paths` and `path`, are relative to the repository root, as the file objects give them. `paths` matches whole path components (`src` matches `src/lib.rs`, not `srcs/`) and takes no globs.

### `changed_files`

| Input | Default | Meaning |
|---|---|---|
| `range` | uncommitted changes | What to compare. |
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
| `syntax` | `false` | Diff by syntax: `moved` and `reformatted` marks, emphasis on changed tokens only, and the classification. |
| `context` | 3 | Unchanged lines around each change, 0 to 10000. |
| `max_lines` | 400 | Lines per result, across hunks, 1 to 20000. |
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
| `cursor` | | The previous page's `next_cursor`. |

The result:

```json
{"header":{…},"files":[…],"truncated_files":[{"path":"big.c","remaining_lines":840,"cursor":"77aa…"}],"total_files":31,"next_cursor":"0d4e…"}
```

`truncated_files` lists the files of this page cut short. Each entry's `cursor` continues that file with `file_diff`, given the same `path`, `range`, `syntax` and `context`.

## Results

A successful call returns two things:

- **`structuredContent`:** the objects described above.
- **`content`:** the same result as text, for clients that show the model only text.

The text is a unified diff with a status line per file, a second column for the syntax diff's marks (`>` moved, `~` reformatted), and the cursor to continue with when there is one:

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

## Paging and truncation

- **Pages of files:** `changed_files` and `diff` return files in the order printed output lists them (git's path order, renames paired). `next_cursor` is null on the last page.
- **Long files:** `file_diff` and `diff` cut a long file at a hunk boundary. A hunk too large for a whole result is cut at a line boundary. A hunk cut short gets `old_start`, `old_lines`, `new_start` and `new_lines` for the lines it holds, so every hunk in a result is consistent on its own. The parts of a file, read in order, hold each of its lines once.
- **Progress:** every result holds at least one line or one file, so following `next_cursor` always reaches the end.
- **Cursors:** a cursor is opaque. It continues only the request that returned it: the same tool and arguments, except `cursor`, `limit`, `max_files` and the line limits, which may change between pages.
- **Stale cursors:** a cursor is refused with a tool error when its request's arguments differ, or when the diff changed since. A diff changes when a revision now names another commit, a file joins or leaves the list, or, for `file_diff`, the file's content changes. Call again without `cursor` to start over.

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

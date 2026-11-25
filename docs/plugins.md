# Plugins

Plugins are programs on `PATH` that dffy starts and talks to over stdin and stdout. This page covers installing them and the three protocols, for users and for plugin authors.

| Kind | Program | Does |
|---|---|---|
| [Renderer](#renderer-plugins) | `dffy-render-<name>` | Draws a file in its rendered form. |
| [Review](#review-plugins) | `dffy-review-<name>` | Keeps review comments in step with a code host. |
| [Agent](#agent-plugins) | `dffy-agent` | Asks a coding agent about a review thread. |

Plugins are experimental, and their protocols and settings may change. None is built in or installed by default.

## Renderer plugins

A renderer draws a file in its rendered form, such as Markdown with its headings, lists and tables, or CSV as rows of cells. dffy sends it both sides of a changed file and gets back each side as blocks of styled lines. dffy diffs those blocks, and the viewer then works as on any diff:

- Changed blocks are tinted, and an edited paragraph shows the words that changed.
- A moved block is marked moved (`»`).
- `n`/`N` go between changed blocks, `/` searches the rendered text, and `x`/`X` reveal unchanged blocks.

### Enabling a renderer

1. Install it. This repository has two renderers:

   | Plugin | Draws |
   |---|---|
   | `dffy-render-markdown` | Markdown files, with ```` ```mermaid ```` fences drawn as diagrams. |
   | `dffy-render-mermaid` | Mermaid files (`.mmd`, `.mermaid`): [flowcharts and sequence diagrams](#the-mermaid-plugin) drawn in text. |

   Each dffy release ships both as separate downloads:

   ```sh
   brew install --cask \
     amalucelli/tap/dffy-render-markdown \
     amalucelli/tap/dffy-render-mermaid
   # or
   curl -fsSL https://raw.githubusercontent.com/amalucelli/dffy/main/install.sh | sh -s -- \
     --plugin markdown \
     --plugin mermaid
   ```

   The program must be on `PATH`. [install.md](install.md#plugins) also covers `cargo install --git`. From a clone, `task install:plugins` puts every plugin in `~/.local/bin`.

2. Map file types to it by extension in the config file:

   ```
   render = md:markdown
   ```

   Each pair is `extension:name`, and the name picks the program `dffy-render-<name>` on `PATH`. Separate pairs with spaces. `render = md:markdown markdown:markdown` also covers `.markdown` files, and `render = md:markdown mmd:mermaid mermaid:mermaid` adds Mermaid files.

3. Press `r` on a file of that type in the viewer, and `r` again to go back to the lines. The status bar says `rendered` while it's on.

   dffy remembers the choice (`rendered` in the state file). It applies to every file whose type has a renderer. Other files keep their lines.

`dffy doctor` lists each configured renderer and whether its program is on `PATH`, with `!` when it isn't.

In the rendered view:

- Lines wrap between words whatever the `wrap` setting says, since renderers leave a paragraph on one line. A word wider than the pane breaks after a hyphen or slash, or wherever it must.
- The gutter numbers each block with the source line it starts on. Hunk headers (`@@ -a,b +c,d @@`) and `⋯ N unchanged lines` count source lines too, including the blank lines between blocks.
- A copy (`v`, `y`, or a mouse drag) holds the source of every selected block, not the rendered text.
- `e` opens the editor at the source line of the block at the top.

When a file can't be rendered, its lines show and the status bar says why:

- No renderer handles its type.
- The program isn't on `PATH`.
- The renderer failed.
- The file is binary, too large, or not UTF-8 text. dffy never sends such a file.

A diff piped to dffy has only its hunks, so there's nothing to render.

### The Mermaid plugin

`dffy-render-mermaid` draws a Mermaid file's diagram with box characters, so its diff reads as a diagram. A file holds one diagram. The Markdown plugin draws ```` ```mermaid ```` fences the same way.

- **Flowcharts** (`graph`, `flowchart`; `TD`/`TB`, `BT`, `LR`, `RL`):
  - Nodes with their labels, in shapes for `[rect]`, `(round)`, `([stadium])`, `((circle))`, `[[subroutine]]`, `[(cylinder)]`, `{diamond}`, `{{hexagon}}`, `>flag]` and `[/lean/]`. Any other shape is a box.
  - Edges `-->`, `---`, `-.->`, `==>`, `--o`, `--x`, `<-->` and `~~~` (spacing only). Labels with `-->|text|` or `-- text -->`. Chains (`A --> B --> C`) and groups with `&`.
  - An edge from a node to itself shows as `↻` in the node.
  - `subgraph … end` is a titled frame. A frame that would overlap another frame or a node outside it is left out, and a line under the drawing lists its nodes instead.
  - `classDef`, `class`, `style`, `linkStyle` and `click` are skipped. An edge to a subgraph goes to a node with the subgraph's name.
- **Sequence diagrams** (`sequenceDiagram`):
  - A lifeline per `participant` or `actor`, with `as` aliases. An actor's box has round corners.
  - Messages `->`, `-->`, `->>`, `-->>`, `-x`, `--x`, `-)`, `--)`, `<<->>` and `<<-->>`. A message to oneself is a loop. `autonumber` works.
  - Notes `left of`, `right of` and `over` one or two participants.
  - `loop`, `alt`/`else`, `opt`, `par`/`and`, `critical`/`option` and `break` as titled frames.
  - `activate`, `deactivate`, `box` groups and `rect` highlights are skipped.
- **Anything else** shows as its source lines, a block per line, so its diff is a line diff. So does a diagram that doesn't parse, and one past the size limits:

  | Diagram | Limit |
  |---|---|
  | Flowchart | 300 nodes, 600 edges, 100 subgraphs, or 3000 nodes and edge bends |
  | Sequence diagram | 60 participants or 2000 statements |
  | Any | a drawing over a million characters |

  Node ids are letters, digits and `_`. An id with `-` or `.` doesn't parse.

How a changed diagram shows:

- A flowchart is one block. When it changes, the old and new drawings show side by side, with changed words emphasized. An edge label edited within the room it had changes only its own line. An edit that needs more room, such as a longer node label, moves what's beside or after it, and those lines show as changed too.
- A sequence diagram is a block per statement: the participants, each message, each note, and each frame's top, divider and bottom. An added or edited message shows alone. A message long enough to widen a column also changes every arrow that crosses that column.

The plugin fits diagrams to the pane's width by wrapping labels and messages. A diagram that's still wider keeps its width, and dffy wraps its lines.

### The renderer protocol, version 1

A renderer is an executable named `dffy-render-<name>`. dffy runs it with no arguments, writes one JSON request to its stdin and closes it, then reads one JSON answer from its stdout.

#### The request

```json
{
  "version": 1,
  "path": "docs/README.md",
  "old": "# Title\n\nOld text.\n",
  "new": "# Title\n\nNew text.\n",
  "width": 72,
  "background": "dark"
}
```

| Field | Meaning |
|---|---|
| `version` | The protocol version, `1`. |
| `path` | The file's path in the repository. |
| `old`, `new` | The two sides' whole text, or `null` for a side that doesn't exist (an added or deleted file). |
| `width` | Roughly the columns a pane has for text. Wrap tables and other wide blocks to it. |
| `background` | `dark` or `light`, the terminal's background, for renderers that pick colors. |

#### The answer

```json
{
  "version": 1,
  "old": [
    {"kind": "heading", "source": [0, 1], "lines": [[{"text": "Title", "style": ["heading"], "level": 1}]]},
    {"kind": "paragraph", "source": [2, 3], "lines": [["Old text."]]}
  ],
  "new": [
    {"kind": "heading", "source": [0, 1], "lines": [[{"text": "Title", "style": ["heading"], "level": 1}]]},
    {"kind": "paragraph", "source": [2, 3], "lines": [["New ", {"text": "text", "style": ["bold"]}, "."]]}
  ]
}
```

`old` and `new` are each a list of blocks, or `null` when the request's side was `null`. A block is a piece of the document that only changes whole: a heading, a paragraph, a list item, a table row.

| Block field | Meaning |
|---|---|
| `kind` | A word for what it is (`heading`, `paragraph`, `list-item`, `code`, `table-row`...). Any word works. It's part of the block's identity. |
| `source` | `[start, end]`, the 0-based lines of that side's text the block comes from, `end` excluded. It must lie within the side's lines. An empty range (`[n, n]`) is a block drawn from no source line, such as the blank line between two blocks. It gets no line number and is never marked moved. Make that blank line a block of its own, not the last line of the block before it. Otherwise adding a block after that one changes it too. |
| `lines` | The block's lines on screen, each a list of spans. A span is a string, or an object with `text`, a `style` list and, for a link, its `url`. No span may hold a line break. |
| `language` | Optional, for a code block: the language of its text (`rust`, `py`, `sh`). dffy highlights it with its own highlighter when it knows the language. |

Span styles are names:

| Style | Drawn as |
|---|---|
| `bold`, `italic`, `underline`, `dim` | That attribute. |
| `code` | The theme's string color. |
| `link` | Underlined, in the theme's function color. Give the target in the span's `url`. |
| `heading` | Bold. With the span's `level` 1 also underlined, and levels 1 and 2 in the theme's keyword color. |
| `syntax-<role>` | A theme's syntax color: `syntax-keyword`, `syntax-string`, `syntax-comment`, `syntax-function`, `syntax-type`, `syntax-number`, `syntax-constant`, and the other roles in [themes.md](themes.md). |

- Unknown names and fields are ignored.
- Escape sequences aren't interpreted. Text is drawn as text, with control characters shown escaped.
- To fail, answer `{"version": 1, "error": "what went wrong"}`. dffy shows the message whatever the exit status. Any other answer counts only from a renderer that exits with status 0.

#### How dffy diffs blocks

- **Identity:** a block's `kind`, its words with each one's style and link target, and its `source` lines, ignoring trailing whitespace.
- **Diff:** dffy diffs the two lists of identities with its line diff. A block is unchanged only when its source is. How the rendered text is spaced or wrapped doesn't count.
- **Changed blocks** are removed or added whole.
- **Pairing:** within each run of changed blocks, a removed block pairs with an added block of the same kind whose text is similar enough. The test is the one that pairs edited lines in a line diff.
- **Word emphasis:** in a pair, a word whose text, style or link target changed is emphasized.
- **Moved:** a changed block whose identity is also among the other side's changed blocks is marked moved.

#### Versions

- Both the request and the answer carry `version`.
- dffy refuses an answer in a version it doesn't speak, and shows the lines with a note. A renderer should answer a request in a version it doesn't speak with an error.
- Unlike the review protocol, this one isn't experimental. Renderers are written outside this repository, so it doesn't change in place ([design.md](design.md#protocols)). A change that would break existing renderers bumps the version. New optional fields and new style names don't.

### Writing your own

A renderer can be written in any language. It reads all of stdin, renders each side that isn't `null`, and prints the answer.

- Blocks are the unit of change. Make each one as small as one change a reader would notice: a paragraph, an item, a row. A whole table as one block shows a one-cell edit as the whole table replaced.
- Leave paragraphs on one line. dffy wraps them, and word emphasis works best on a paragraph diffed as one line. Wrap only what has its own layout, like tables, to `width`.
- Answer within 500ms ([Timeouts and failures](#timeouts-and-failures)).
- Point `source` at the right lines. Copies and `e` use it.

This renderer shows CSV files as rows of cells. Save it as `dffy-render-csv`, make it executable, put it on `PATH`, and add `render = csv:csv` to the config:

```python
#!/usr/bin/env python3
"""dffy renderer for CSV: each row a block, cells between bars, the
header row bold. Assumes one row per line (no quoted line breaks)."""
import csv, io, json, sys

def blocks(text):
    if text is None:
        return None
    out = []
    for n, row in enumerate(csv.reader(io.StringIO(text))):
        line = []
        for i, cell in enumerate(row):
            if i:
                line.append({"text": " │ ", "style": ["dim"]})
            line.append({"text": cell, "style": ["bold"]} if n == 0 else cell)
        out.append({"kind": "row", "source": [n, n + 1], "lines": [line]})
    return out

request = json.load(sys.stdin)
if request.get("version") != 1:
    print(json.dumps({"version": 1, "error": "this renderer speaks protocol 1"}))
    sys.exit(1)
print(json.dumps({"version": 1, "old": blocks(request["old"]), "new": blocks(request["new"])}))
```

`plugins/markdown` is a fuller example in Rust, on pulldown-cmark. It draws lists with bullets and numbers, quotes with a `│` bar, tables with box characters wrapped to the width, links underlined, images as `[image: alt]`, HTML dim, and Mermaid fences through the Mermaid plugin's library. `plugins/mermaid` lays out graphs itself, with no dependency beyond JSON and character widths.

### Performance and safety

#### When renderers run

- Without a `render` setting, nothing about renderers happens.
- With one, dffy looks for a renderer's program on `PATH` only when a file of its type is about to show rendered: when you press `r`, or when such a file comes up with the rendered view remembered as on.
- dffy keeps the answer for the session.

#### The viewer never waits

- A renderer runs in the background. The file's lines show until its rendering arrives.
- Moving to another file, pressing `r` again or quitting stops a running renderer and every process it started.

#### Timeouts and failures

- A renderer that takes longer than 500ms is killed with every process it started. The file keeps its lines, with a note.
- The same happens when a renderer crashes, prints something that isn't a valid answer, or answers in another protocol version.
- dffy doesn't retry a failed file until the file changes or `r` turns rendering on again.

#### Caching

dffy keeps answers in its disk cache (`cache`), so reopening a file doesn't run the renderer again. The key covers:

- the renderer's name, and its program's modification time and size, so replacing the program invalidates its answers
- both sides' contents
- the width and the background

#### What can run

- Only programs you installed run, and only `dffy-render-<name>` for a name your config lists.
- Names are lowercase letters, digits, `-` and `_`, so the setting can't point at another path.
- Relative or empty `PATH` entries are skipped, so the repository being viewed can't supply a renderer.
- stderr is discarded, since it would land on the viewer's screen.
- Answers over 64MB are refused.

## Review plugins

A review plugin connects the [review comments](comments.md) to a code host: GitHub, GitLab, Gerrit, or anything else that holds reviews. The plugin talks to the host, and dffy never touches the network.

dffy runs one plugin process in the background for as long as it shows the repository ([the protocol](#the-review-protocol-version-1)). dffy keeps the review's comments file. It sends each request what it needs of the comments and writes what the plugin sends back. The plugin never prompts or draws. dffy shows its progress and errors.

### Enabling one

1. Install it. `dffy-review-github` comes from this repository, and [install.md](install.md#plugins) has every way to install it. It needs [`gh`](https://cli.github.com), logged in.

   ```sh
   brew install --cask amalucelli/tap/dffy-review-github
   # or
   curl -fsSL https://raw.githubusercontent.com/amalucelli/dffy/main/install.sh | sh -s -- --plugin review-github
   ```

2. Open a review with the plugin's command: `dffy pr <number>` for dffy-review-github, or `dffy pr` for the current branch's. [viewer.md](viewer.md#code-host-reviews) covers reviewing it.

With `review` unset, or `review = auto`, dffy uses the only `dffy-review-<name>` program on `PATH`. With several installed it uses none, and `dffy doctor` lists them. `review = <name>` picks one, `review = none` turns review plugins off, and `--review <name|auto|none>` does either for one run. The flag wins over the config.

`dffy doctor` lists the review plugin in use, whether its program is on `PATH`, the command it adds, and the plugin's own checks under it, with `!` for each that fails. For dffy-review-github the checks are whether `gh` is installed and logged in.

### The review command

The plugin names the command in its `initialize` answer: `dffy pr` for dffy-review-github, perhaps `mr` for a GitLab plugin. dffy adds the command only while there's a review plugin and it names one. Otherwise `dffy pr` reads `pr` as a revision, and plain `dffy` doesn't ask for the branch's review.

The plugin's words appear wherever dffy names what it reviews: the noun ("pull request", "pull requests") in the command's help, the command panel and the picker, the short form and id prefix in labels (`PR #7`), and its events in the post dialog. Everything else, from opening a review to posting it, is dffy's.

- dffy refuses a command that isn't lowercase letters, digits and `-` starting with a letter, or that's one of its own subcommands or their aliases (`diff`, `show`, `config`, `doctor`, `help`…). `dffy doctor` says why there's no command.
- When the arguments could name the command (help, or a first word that isn't a dffy subcommand and could be a command name), dffy starts the plugin for its `initialize` before reading them, and stops it once answered. Otherwise the viewer asks once its first frame is out, without waiting. Other runs ask only when they use the plugin.
- Under `auto`, dffy looks for the plugin at those same moments, and printed output never looks. The names it finds are cached on disk (`cache`), keyed by `PATH` and each of its directories' modification time, so a later run costs a look at each directory. Adding, removing or renaming a program in one lists them again. `dffy doctor` always lists them again.
- The answer is cached on disk (`cache`), keyed by the plugin program's path, modification time and size. Replacing the program asks again, and every `initialize` dffy sends refreshes the entry.
- A failure, or no answer within 2 seconds, is cached too, as why there's no command. When the command's word is then read as a revision, the error names the plugin.
- `dffy doctor` always asks again, and caches what it's told.

### dffy-review-github

`dffy-review-github` reviews a GitHub pull request in dffy, through `gh` and git. [review-github.md](review-github.md) covers how it opens, imports, posts and watches a PR, and what it keeps.

### The review protocol, version 1

The protocol is experimental and may change. It knows nothing of any host. The plugin names its reviews, events, labels and groups, and dffy draws them.

**Designed for.** Each message is checked against these before it's added or changed:

- GitHub pull requests, through `gh`: a pending review that a submitting event sends, and threads resolved by id.
- GitLab merge requests, through `glab`: draft notes that a submitting event publishes together, discussions resolved by id, and an approval as an event.
- Gerrit changes: a patch set as the session's head, and a newer one as `head/moved`. Drafts published with a reply. Comments ported from older patch sets, placed by their anchors. Votes on several labels, which a session names and a `post`'s answer repeats once it changed one. A thread resolved by a reply that says so, which the plugin sends from `resolves`.
- A review with no host, kept in the repository: no `url`, no `watch`, and its own `detect` and `list`.

**Program.** An executable named `dffy-review-<name>`, found on `PATH` as renderers are. Names are lowercase letters, digits, `-` and `_`, and relative or empty `PATH` entries are skipped.

**Framing.** [JSON-RPC 2.0](https://www.jsonrpc.org/specification) over the plugin's stdin and stdout, one message per line: a JSON object with its line breaks escaped as JSON escapes them in strings, then `\n`. Lines instead of `Content-Length` headers let any language read messages with its line reader, a shell script included. Requests carry an `id` (dffy's are numbers) and get one answer each, a `result` or an `error`. Notifications carry no `id` and get no answer. stderr isn't shown, except that its last line goes in dffy's message when the process exits.

**Lifecycle.**

- **One process per repository.** The viewer starts the plugin on first use, after its first frame: to look for the branch's review, to open one, or for "Open review…". `dffy pr` starts it before the viewer, which takes it over, so opening a review costs one process start.
- **No terminal.** dffy starts it in the root of the working tree, in a session of its own, so a passphrase or password prompt fails instead of reaching the screen. The session is also its process group, which dffy kills when it quits.
- **`initialize` first.** Requests made before its answer wait for it. A plugin that answers it with an error, or writes anything but a JSON object before answering, is refused. Its requests fail saying why, and it isn't started again.
- **Crashes.** When the process exits otherwise, its unanswered requests fail with how it exited. dffy restarts it after 1 second, then 2, 4, 8 and 16 for crashes in a row, so at most 5 times in a row. A process that ran a minute starts the count over. The new process is told again which review dffy shows and whether you're idle. Requests the old one was running aren't resent: a `post` resumes at the next `P`.
- **Quitting.** dffy sends `shutdown` and `exit`, then kills the process group without waiting. While a `post` runs, the second quit or a signal cancels it, and dffy waits up to 30 seconds for it to end. The plugin stops between drafts, so what it posted gets its refs.
- **Short sessions.** dffy also starts the plugin just for `initialize`, when it needs the naming and the cache has none ([The review command](#the-review-command)), and for `dffy doctor`. These run wherever dffy runs, in a repository or not, and are killed once answered or after 2 seconds (`initialize`) or 3 (`doctor`). `dffy doctor` runs both side by side with the agent plugin's `doctor` and its own checks. One that runs out of time says it didn't answer.

**Environment.** dffy passes its environment on, with `DFFY` set to the path of the dffy that started the plugin.

**Messages.**

| Message | Kind | When |
|---|---|---|
| `initialize {protocol, client}` | request | First. `client` is `{"name":"dffy","version":"…"}`. |
| `list` | request | The review command without an id, and "Open review…" in the command panel. |
| `detect` | request | Plain `dffy` on a branch, without revisions or paths, once the plugin has initialized. The viewer doesn't wait for the answer. |
| `prepare {id, known}` | request | The review command with an id, and a review picked from the list. |
| `pull {id, known}` | request | `p`. |
| `dryRun {id, drafts, resolves}` | request | While there are drafts and no post runs: a moment after your drafts change, and after a `prepare`, `pull` or `post` ends. Also as the `P` dialog opens, when the last answer is older than the drafts. |
| `post {id, event, labels, summary, drafts, resolves}` | request | `enter` in that dialog. When they go, dffy first moves the local review's drafts shown on the review into the review's comments file. |
| `doctor` | request | `dffy doctor`. Never cached. |
| `shutdown` | request | On quit. The answer, `null`, comes once every running request is answered. |
| `exit` | notification | After `shutdown`. The plugin exits. |
| `$/cancelRequest {id}` | notification | Opening another review or updating cancels a running `prepare` or `pull`. The quit or signal that stops a `post` cancels it. dffy also cancels a request it ends before the plugin answers: one whose `session` it can't take, whose operations it can't write, or whose answer nothing waits for any more. |
| `watch {id}` | notification | To a plugin that watches: the review dffy shows, `null` for none. Sent again to a process started after a crash. |
| `idle/changed {idle}` | notification | To a plugin that watches: `true` after 10 minutes without a key on a review, `false` at the next key. |
| `session {request, session}` | from the plugin | During `prepare` or `pull`: the review's [session](#the-session), as soon as its commits are in the clone. dffy opens it at once. |
| `progress {request, progress}` | from the plugin | How far a request got, done of all: `100/312`. The status bar shows it. |
| `comments/apply {request, ops}` | from the plugin | Operations for the review's comments file ([Comments](#comments)). Applied until the plugin answers `request`, even after dffy ended it, and ignored after. |
| `head/moved {id, head}` | from the plugin | The review's head on the host, when it isn't the session's, or no longer. dffy says there are new commits, `p` to update. |
| `comments/changed {id}` | from the plugin | The review changed on the host since its comments were imported. dffy says there are new comments, `p` to update. |
| `log {message}` | from the plugin | Words for the status bar, such as why watching failed. |

**Concurrency.**

- dffy drops a push about a review it doesn't show.
- Of `prepare`, `pull` and `post`, one runs at a time. `list`, `detect` and `dryRun` run beside them.
- A `prepare` or `pull` replaces the one running, cancelling it.
- Nothing starts while a `post` runs, and a `post` is refused while a `prepare` or `pull` runs, since both write the review's comments.
- dffy holds a lock on the review from reading a `post`'s drafts until the post ends, so a second viewer's `post` of the same review is refused before it reaches its plugin. A plugin should still refuse a `post` while another runs for the same review, as dffy-review-github does.
- A plugin should answer requests on threads of their own, so a `dryRun` answers while a `prepare` downloads. It should stop a cancelled request where it can, and answer it with error `-32800`.

**Answers.** The examples are dffy-review-github's.

`initialize` answers [the description](#the-description), at once and from nothing but the plugin itself.

`list`:

```json
{"reviews":[{"id":"7","title":"Spell out five","author":"octocat","url":"https://github.com/acme/widgets/pull/7","head_ref":"five","reason":"current_branch"}]}
```

- Each review has an `id` and an optional `label`, as a session does.
- `reason` is `current_branch`, `review_requested` or `mine`. dffy titles these and lists them first, in that order.
- A review listed for another reason may carry a `group`, the title it's listed under. Groups come in the order they first appear, and a review without one goes under `Other`.
- Each review should be listed once.
- dffy counts your drafts in each review's comments file itself.

`detect` answers `{"review":{…}}` with the session of the open review of what's checked out, or `{"review":null}`. That's the branch's review, or whichever the host ties to the commit, as Gerrit does by its Change-Id. It runs on every start of `dffy`, so it should answer from what the plugin keeps when it can. When it names a review, dffy prepares it and keeps showing what plain `dffy` shows, with the review's comments and status. The whole review (`--merge-base <base>` against the working tree) is a command away.

`prepare` sends a `session` notification, then `progress` for each page of threads written (threads so far, of all of them), and answers `{}` once the threads are in. When it answered the session from what it kept and the head turns out not to be the host's, it pushes `head/moved`. `pull` does the same after fetching the head, with a session on the new head.

`dryRun` answers `{"drafts":[…]}`, a [plan](#a-plan) for each draft in the order they came. It sends and writes nothing. dffy counts the plans, what's pending on the host and the resolves waiting itself.

`post` sends `progress` for each draft, then answers:

```
{"unposted":[…],"posted":9,"failed":3,"pending":4,"url":"https://github.com/acme/widgets/pull/7#pullrequestreview-2001","last_reviewed_head":"<commit>"}
```

`unposted` holds a plan for each draft not posted, all `skip`. The counts follow, and the review's `labels` when the post changed a vote ([The post request](#the-post-request) has every field). With no drafts and an event that `submits`, it submits what's pending on the host.

`doctor`:

```json
{"checks":[{"name":"gh","ok":true,"detail":"gh version 2.62.0","brief":"gh 2.62.0"},{"name":"auth","ok":false,"detail":"not logged in"}]}
```

| Check field | Meaning |
|---|---|
| `name` | A short name for what the plugin needs. |
| `ok` | Whether it's fine. |
| `detail` | A line saying what was found. |
| `info` | Optional. `true` on a check that's fine but only notes something. `dffy doctor` marks it `·` rather than `✓`. |
| `brief` | Optional. The few words `dffy doctor` folds into the plugin's one row, in place of `detail`: `review  github · gh 2.62.0 · logged in as octocat`. |

`dffy doctor --verbose` shows each check on its own line with its `detail`. The plugin should answer within 3 seconds. Otherwise dffy says it didn't answer and shows none of its checks.

**Errors.** A request that fails answers with JSON-RPC's `error`: `{"code":…,"message":"…","data":…}`. dffy shows the message.

| Code | Meaning |
|---|---|
| `-32601` | A method the plugin doesn't have. dffy says the plugin doesn't know the request and may be older than dffy. |
| `-32602` | A request the plugin can't take, such as an unknown event or an id that isn't one of its own. |
| `-32800` | Cancelled. |

- `data`, when there is one, says what was done before the failure, in the result's shape. A `post` stopped midway says what it posted.
- An error after a `session` notification leaves the session good, since the code is in the clone. It says what's missing, such as the threads, and the viewer stays open. Before a session, the review command exits with the error.

**Compatibility.**

- `initialize` names the protocol both ways. dffy refuses a plugin that speaks another, and dffy-review-github refuses a dffy that speaks another. Each says which it speaks.
- Both sides ignore fields and notifications they don't know. A request a plugin doesn't have answers `-32601`.
- A plugin that can't initialize adds no command and isn't asked for the branch's review, and `dffy doctor` says so. One without `doctor` shows no checks of its own.
- While the protocol is experimental, it changes in place, without a new version number. dffy and the plugins in this repository change with it.

#### Comments

dffy keeps each review's comments file ([comments.md](comments.md#where-the-file-is)). The plugin never reads or writes it. A request carries only what it needs of the file, as dffy read it when it sent the request.

**The plugin's fields.** A comment from a host may hold fields of the plugin's own beside dffy's, prefixed with the plugin's name, such as dffy-review-github's `github_thread`. dffy keeps them and hands them back wherever a request names that comment: in `known`, in a reply's `in_reply_to`, and in `resolves`.

**`known`**, in `prepare` and `pull`: an entry for each comment that has a `ref`, for the plugin to merge what it imports. It's empty while dffy has no file for the review.

```json
{"ref":"github:PRRC_kwDOA","anchored":true,"pending":true,"github_thread":"PRRT_kwDOA"}
```

| Field | Meaning |
|---|---|
| `ref` | The comment's. |
| `anchored` | `true` when it has its lines and anchor. An import that can't read its lines again can leave them as they are. |
| `pending` | `true` while it's marked `pending`. |
| others | The plugin's fields. |

An import sends each thread's `resolved` as the host has it, with `host_resolved`. dffy keeps a resolve or reopen made in dffy and not sent through it, even one made after the request went. Only `host_resolved` changes then.

**`drafts`**, in `dryRun` and `post`: your [drafts](comments.md#drafts), in the file's order. A dry run then adds those of the local review's file shown on the review, leaving out its threads on other branches. A comment by someone else, such as an agent, is never sent, because it would go out under your name.

```json
{"id":"9f3c2a71b04e5d18","path":"src/lib.rs","side":"new","start":{"old":12,"new":12},"end":{"old":null,"new":14},"text":"This can overflow.","anchor":{"before":["…"],"lines":["…"],"after":["…"]},"distance":0}
{"id":"1b7e94c0d25a6f03","path":"src/lib.rs","text":"Same here?","in_reply_to":{"ref":"github:PRRC_kwDOA","github_thread":"PRRT_kwDOA"}}
```

A draft has whichever of the comments file's `id`, `path`, `old_path`, `side`, `start_side`, `start`, `end`, `anchor`, `distance`, `text` and `suggestion` it holds, and:

| Field | Meaning |
|---|---|
| `resolved` | `true` on a thread's first comment resolved before it's posted. It's posted, then resolved. |
| `in_reply_to` | For a reply, its thread's first comment: `{"ref":…}` with the plugin's fields when that's posted, else `{"id":…}`, a draft earlier in the list. |

**`resolves`**, in `dryRun` and `post`: the posted threads resolved or reopened in dffy and not sent. Each holds its first comment's `ref`, the state to send, where the thread is (for words about it), and the plugin's fields:

```json
{"ref":"github:PRRC_kwDOA","resolved":true,"path":"src/lib.rs","side":"new","end":{"old":null,"new":14},"github_thread":"PRRT_kwDOA"}
```

**Out.** The plugin sends what it would change as `comments/apply`: a list of [`dffy comments apply`'s operations](comments.md#operations). dffy applies them to the review's file as they come, all or none, under its lock, and before it reads anything the request says after them.

- `upsert` by `ref` merges a host's comment field by field, the plugin's fields included. `set` and `rm` name a comment by `id` or `ref`.
- `host_resolved` on a thread's first comment says whether the host has the thread resolved, as the plugin last saw. An import sets it, and so does sending a resolve. It's how dffy knows what goes in `resolves`.
- `gone` names the refs of `known` the host no longer has, once the plugin has read every comment. dffy removes them by its rules, keeping a thread that holds your drafts.
- A `set` can't give a comment that has a `ref` another one. Clearing it first can.
- When dffy can't apply some operations, as when a `set` names a draft that's gone, it cancels the request, which ends saying why. What the plugin sends until it answers is still applied, so a post's refs aren't lost. The same holds for a request dffy ended for another reason, and for one cut short by the plugin exiting. What it sent before is in the file, and the next `post` doesn't send those drafts again.
- Operations for a request the plugin has answered are ignored.
- The viewer doesn't change the comments while a `post` runs, so what it sent is what the plugin writes back about.

**Posting.** A `post` sends each draft's `ref`, `url` and `pending` as soon as the host has it, so a second `post` never sends that draft again. A posted comment is marked `"pending":true` while it's in your pending review on the host. A draft the host refused gets the reason in its `post_error`, which goes once it's posted. When a `post` with an event that `submits` ends well, dffy clears every `pending` itself.

**Whole runs.** A message about a whole run that should outlast the status bar, such as why a post stopped, goes in the file's `notice` (`{"op":"notice","notice":"…"}`). dffy shows it when it reads the file.

#### The description

```json
{"protocol":1,"command":"pr","noun":"pull request","nouns":"pull requests","short":"PR","id_prefix":"#","events":[{"id":"pending","key":"p","label":"pending: on GitHub, for you only","done":"review pending","submits":false},{"id":"approve","key":"a","label":"approve","done":"approved"}],"outdated_reply_warning":"GitHub collapses this thread, so a reply there is easy to miss","capabilities":{"watch":true}}
```

| Field | Meaning |
|---|---|
| `protocol` | `1`, the protocol it speaks. dffy refuses another, and says which it speaks. |
| `command` | The subcommand dffy adds: lowercase letters, digits and `-`, starting with a letter. |
| `noun`, `nouns` | What it reviews, singular and plural, up to 40 printable characters each. |
| `short` | Optional. What goes before a review's id in labels, `PR` in `PR #7`, up to 40 printable characters. The noun when left out. |
| `id_prefix` | Optional. What goes right before the id, `#` in `PR #7`, up to 8 characters. Nothing when left out. |
| `events` | Optional. The post dialog's choices, in order, 1 to 9 (fields below). |
| `outdated_reply_warning` | Optional. What dffy says, once, before you reply to a thread the host marks outdated, up to 200 printable characters. Nothing when left out. |
| `capabilities` | Optional. `{"watch":true}` for a plugin that watches the review dffy names in `watch`, pushes `head/moved` and `comments/changed`, and pauses while you're idle. dffy sends `watch` and `idle/changed` only to such a plugin, and never asks about a review on a timer of its own. |

| Event field | Meaning |
|---|---|
| `id` | Sent as `post`'s `event`. Up to 40 lowercase letters, digits, `_` and `-`, starting with a letter. |
| `key` | One lowercase letter or digit that picks it. Not `j`, `k`, `l`, `s` or `v`, which the dialog takes. |
| `label` | Shown beside the key, up to 40 printable characters. |
| `done` | Shown in the status bar once it went (`approved`), up to 40 printable characters. |
| `submits` | `false` for an event that leaves the review open on the host, such as a pending review. `true` when left out. |

No ids or keys repeat. Without `events`, there's one: `{"id":"post","key":"p","label":"post","done":"posted"}`.

dffy refuses a description that breaks these, and `dffy doctor` says why.

#### The session

```json
{"id":"7","title":"Spell out five","url":"https://github.com/acme/widgets/pull/7","base":"<commit>","head":"<commit>","last_reviewed_head":null,"state":"open","checkout":"gh pr checkout 7"}
```

| Field | Meaning |
|---|---|
| `id` | The review, as the plugin's requests take it: a string, one printable line of up to 200 bytes, not starting with `-`. It names the review's comments file too. |
| `label` | Optional. What to show for the id when it isn't the id itself, such as a change number for a long change id. One printable line of up to 200 bytes. Labels read `<short> <id_prefix><label or id>`. |
| `title`, `url` | The review's. `null` when the host couldn't be reached and nothing was kept from before, or when there's no host to link to. |
| `base`, `head` | Commits in the clone, as full ids: 40 hex digits, or 64 in a SHA-256 repository. dffy shows `<base>...<head>`. |
| `last_reviewed_head` | The head of your last review, a full id, or `null`. The plugin keeps it, and says when a `post` changed it. "Since last review" shows `<last_reviewed_head>..<head>`. |
| `state` | Optional. `open`, `closed` or `merged`, as the host last said. |
| `checkout` | Optional. A command that checks the review out, one printable line of up to 200 bytes, such as `gh pr checkout 7`. dffy shows it where something needs HEAD on the review's head, as applying a suggestion does (`check out PR #7 first: gh pr checkout 7`). dffy never runs it. Without it, dffy says HEAD isn't the review's head. |
| `labels` | Optional. Up to 9 labels this review is voted on beside the event, for a host whose verdict is more than one choice, such as a Gerrit change's Code-Review and Verified. None when left out. |

Labels belong to the review, so the session names them, not `initialize`: each review may have its own, and your votes on it. Each label looks like this:

```json
{"name":"Code-Review","values":[{"value":-1,"label":"I would prefer not"},{"value":0,"label":"No score"},{"value":1,"label":"Looks good to me"}],"vote":0}
```

| Label field | Meaning |
|---|---|
| `name` | Up to 40 printable characters. |
| `values` | 1 to 9, in the order offered. Each is a whole-number `value` with words in `label`, up to 60 printable characters. |
| `vote` | Your vote on the host now, one of the values. The neutral value when you haven't voted. |

No names or values repeat. The post dialog shows each label under the events, starting on your vote. j/k go to a label and ←/→ change it.

dffy refuses a session whose `base`, `head` or `last_reviewed_head` isn't a full commit id, or whose labels break these.

#### A plan

```json
{"id":"9f3c2a71b04e5d18","path":"src/lib.rs","plan":"line","line":13,"side":"new"}
```

| Field | Meaning |
|---|---|
| `id` | The draft's `id` in the comments file. |
| `path` | Its file. |
| `plan` | `line`: on its lines, or a reply in its thread. `file`: a comment on the whole file. `skip`: not posted, so it stays a draft. |
| `line`, `side` | Where it goes at the review's head, when it has lines: the last line, on the `old` or `new` side. |
| `unsure` | `true` when the plugin chose `line` or `file` without the host's own diff of the file, so the host may disagree. A line comment may be refused, or a file comment may have fit on its lines. Left out otherwise. dffy doesn't read it yet. |
| `reason` | For `skip`: why, as the user reads it. |

- A dry run skips only a draft that can't be posted yet, because what it's on isn't on the host yet: its lines, its file, or its thread's first comment.
- A draft the host would refuse for another reason is planned, refused while posting, and gets a `post_error`. `post`'s `unposted` lists it as `skip` too, with the host's reason.
- The viewer heads a draft planned `file` with `posts as file comment`, and one planned `skip` with its `reason`, or `not in the review yet` without one.

#### The post request

- `event` is the `id` of one of the described events, or `post` when none are described.
- `summary` is the review's summary, left out without one. dffy keeps the summary being written in the comments file's `summary`, and removes it once a post with an event that `submits` ends well.
- `labels` holds, by name, the votes in the dialog that differ from the session's `vote`: `{"Code-Review":1}`. A label left on your vote isn't sent, so the vote given before stays. Without a change there's no `labels`.
- The plugin posts the `drafts` it's sent, and sends the `resolves`.

The result:

- `failed` counts the drafts that weren't posted: skipped by the plan, or refused by the host. Neither holds the event back. The review goes without them.
- `pending` counts the comments your pending review on the host had before the run, which a submitting event sent with the drafts.
- `url` is the review's link. dffy opens it with `o`.
- `last_reviewed_head`, a full id, when the post recorded a new head as reviewed. A post that posted a comment or applied an event that `submits` should record one. dffy takes it from there, and the plugin's next session should say the same. dffy never works it out itself. Left out when the post recorded none.
- `labels`, in the session's shape, when the post changed a vote: the review's labels with your votes as the host has them now. dffy takes them as the session's, so the next dialog starts on them and doesn't send a vote that's already given. When left out, the session's stay as they were.
- When the post ends, dffy reads the comments file. Its `notice`, if any, shows over dffy's own words for how the post went.

## Agent plugins

The agent plugin, `dffy-agent`, asks a coding agent you already run about a review thread: Claude Code (`claude`), Codex (`codex`), Pi (`pi`), or a command of yours. It returns the answer and, when the answer proposes a change, its suggestion. dffy puts both in the thread as a reply authored by the agent.

The agent can read and search, but can't write, run arbitrary commands or reach the network. Its recipe, how it's run, enforces that, not the prompt. Neither the plugin nor the agent writes a file.

For [Fix with agent](viewer.md#fixing-with-an-agent), the plugin also says how to start each installed agent interactively on a thread, in its normal mode. Claude Code, Codex and Pi get the thread's prompt as their argument, and Amp gets it on the clipboard. dffy knows no agent by name, so Fix with agent needs the plugin too.

`dffy-agent` comes from this repository, and [install.md](install.md#plugins) has every way to install it:

```sh
brew install --cask amalucelli/tap/dffy-agent
# or
curl -fsSL https://raw.githubusercontent.com/amalucelli/dffy/main/install.sh | sh -s -- --plugin agent
```

It needs `claude`, `codex` or `pi` on `PATH`, logged in, and nothing else: no SDK, no Node.

### The agents and their recipes

The recipes target each CLI's current release. An agent is available when its program is on `PATH`. No version is checked: when a CLI release breaks a recipe, the recipe gets fixed in the plugin. `--version` and `--help` run only for `doctor`, to show what the CLI says and whether it knows the recipe's flags. Every recipe gets the prompt on stdin, never in its arguments, and runs in the working tree. None saves a session.

- **Claude**: `claude -p --verbose --output-format stream-json --tools "Read,Grep,Glob" --disallowedTools "mcp__*" --strict-mcp-config --setting-sources user --no-session-persistence --permission-mode dontAsk`. `--tools` leaves only the built-in Read, Grep and Glob, so no edits, no shell and no web. MCP servers' tools are denied separately, and `--strict-mcp-config` loads no server. `--setting-sources user` keeps the reviewed repository's settings, and so its hooks, from loading.
- **Codex**: `codex exec --json --sandbox read-only --ephemeral -c web_search="disabled" -C <worktree>`. It may run commands to read and search, inside the read-only sandbox, which the operating system holds to no writes and no network. Web search is off. Codex runs with your own Codex setup: your MCP servers, hooks, profiles and trusted project config load as in your normal use. MCP servers run outside its sandbox.
- **Pi**: `pi --mode json --no-session --tools read,grep,find,ls --no-mcp --no-approve --offline`. `--tools` leaves only the `read`, `grep`, `find` and `ls` tools, so no edits and no shell. `--no-mcp` connects no MCP server. `--no-approve` ignores the reviewed repository's trust-gated `.pi` config, extensions and skills. `--offline` turns off Pi's own network use, but the model's provider is still reached. Pi has no sandbox: the limit is the tools it's given, not the operating system. With `--offline`, Pi doesn't download the programs its `grep` and `find` tools run, so they work only with `rg` and `fd` installed.
- **A command of yours**: a configured value with a space. Its first word is a name on `PATH` or an absolute path, and the rest are its arguments, split at spaces with no shell. Nothing is enforced: the command's flags are yours.

How each run is read:

| Agent | Replies marked | Answer | Failure | Not logged in |
|---|---|---|---|---|
| Claude | nothing | The final `result` event. | A `result` with `is_error`, its `result` the reason. | `claude auth status` exits 1. |
| Codex | `read-only sandbox` | The last completed item of type `agent_message`. | `turn.failed`, or an `error` event when no answer came. | `codex login status` exits 1. |
| Pi | `read-only tools` | The last assistant message's text. | That message stopped on an error, an abort or the length limit, its `errorMessage` the reason. | stderr says `No API key found` or `No models available`. |
| A command of yours | `custom` | Its stdout. | A non-zero exit. | Not checked. |

- Pi retries a failed message itself, so its last message decides. Pi exits with status 0 either way.
- Pi's own login check (`pi auth check`) needs a provider named, which the recipe leaves to your Pi settings, so the plugin doesn't run it.
- When the output gives no reason for a failure, the reason is the first stderr line that starts with `error:`, as a usage error's does, else the last line.
- The login checks of Claude and Codex run only after a failed run. The error then says how to log in. The plugin runs no login flow.

Amp isn't a built-in agent. It has no read-only mode an ask could rely on: it runs tools without asking, and a repository's `.amp/settings.json` and plugins override restrictions. You can still name it as a command of yours (`agent = amp …`), at your own risk. The plugin lists it for Fix with agent, which starts an agent in its normal interactive mode rather than read-only.

**What stays possible.**

- Every agent can read files outside the working tree, such as `~/.ssh`. No recipe limits reading to the repository.
- The prompt carries the reviewed code, so it can steer what the agent reads and says. With no writes and no network, what the agent read leaves only in the answer: a reply shown in dffy on your machine, never posted to a code host unless you adopt it, and a suggestion only you apply.
- Codex's MCP servers are the exception. The model can call their tools, and they run outside the sandbox, so a server that writes or reaches the network is a way out for an injected prompt.
- dffy's own MCP server is reachable that way when it's one of your Codex servers, and so is the viewer behind it. Its `reply_to_thread` writes a reply by the client, never posted until you adopt it. Its `apply_suggestion`, which writes files, is refused while the viewer asks an agent, as is the control API's `comments/applySuggestion`. `s` in the viewer still applies.
- Your own Claude settings and hooks, in your home directory, load with the user settings.
- Pi's extensions in your home directory (`~/.pi/agent`) load and run inside Pi with your permissions. One could register tools that write or reach the network, or turn tools back on.
- The agent's own instruction files in the repository, such as `AGENTS.md`, are read as text.

The programs are found in absolute `PATH` entries only, so the reviewed repository can't supply one. `--version`, `--help` and the login checks run in `/`.

### The agent protocol, version 1

The protocol is experimental, and changes in place as the review protocol does.

**Program.** The executable `dffy-agent`, found on `PATH` as review plugins are.

**Framing.** As in [the review protocol](#the-review-protocol-version-1): JSON-RPC 2.0 on stdin and stdout, one message per line.

**Lifecycle.**

- **dffy's side.** dffy starts the plugin only when it's needed: on the first ask, on Fix with agent, or for `dffy doctor`. It never starts it at launch, so `initialize` never delays the first frame. Once started, the plugin runs for as long as dffy does.
- **The plugin's side.** Requests run side by side. Each agent runs in a process group of its own, which a cancel kills with everything the agent started. `shutdown`, `exit` and the end of stdin kill every running ask, because killing the plugin's group doesn't reach them. A plugin that crashes kills none. Its agents run on until they next write output, which has no reader then and fails, or until they end. dffy starts the plugin again, and the asks it was running end as failed.

**Messages.**

| Message | Kind | When |
|---|---|---|
| `initialize {protocol, client, agent}` | request | First. dffy passes the config's `agent` value as it is, unread, and leaves `agent` out without one. A name picks the default agent. A value with a space is a command of yours. |
| `ask {agent, prompt, cwd}` | request | An ask on a thread. `agent` is a listed agent's `name`. `cwd` is the working tree, an absolute path. `prompt` is the thread as a prompt ([comments.md](comments.md#as-a-prompt-dffy-comments-prompt)), complete. The plugin sends it to the agent as it is, adding nothing, so the prompt itself ends with the ask for a ```` ```suggestion ```` block. |
| `doctor` | request | `dffy doctor`. |
| `shutdown` | request | On quit. Every running ask is killed, and it answers `null`. |
| `exit` | notification | After `shutdown`. The plugin exits. |
| `$/cancelRequest {id}` | notification | Cancels an ask. Its agent is killed, and the ask answers `-32800`. |
| `progress {request, progress}` | from the plugin | What the agent is doing: `starting…`, `thinking…`, `writing…`, `running a command…`, `reading…`, `searching…`. Sent once each time it changes. |

**Answers.**

`initialize` lists the agents an ask can go to, and the [handoffs](#handoffs):

```json
{"protocol":1,"agents":[{"name":"claude","default":true,"mark":null},{"name":"codex","default":false,"mark":"read-only sandbox"}],"handoffs":[{"name":"claude","program":"/usr/local/bin/claude","args":["{prompt}"]},{"name":"amp","program":"/usr/local/bin/amp","args":[],"prompt":"clipboard"}],"capabilities":{}}
```

- `agents` lists only the agents found that an ask can go to. `name` is `claude`, `codex`, `pi` or `custom`.
- One agent is the `default`: the command of yours when there is one, else the agent the config names, else the first.
- `mark` is how its replies are labeled, `null` for none.
- `handoffs`, optional, lists separately the agents found that a thread can be handed to. Some of them take no asks.

`ask` answers the reply's text and its suggestion:

```json
{"text":"Yes: the sum overflows past u32::MAX.","suggestion":"x.checked_add(y)?"}
```

The answer's last ```` ```suggestion ```` fenced block, closed by a fence at least as long as the one that opened it, is taken out of `text` and becomes `suggestion`. That's its lines without a final line break, as in [the comments file's](comments.md#the-file-version-1) `suggestion`. A fence inside another block opens none. Without such a block there's no `suggestion`. An empty block is an empty suggestion, which removes the lines.

`doctor` answers checks in the review protocol's shape:

```json
{"checks":[{"name":"claude","ok":true,"detail":"2.1.329 (Claude Code)","brief":"2.1.329"},{"name":"claude recipe","ok":true,"detail":"read-only: Read, Grep and Glob only, MCP tools denied, user settings only","brief":"read-only: Read, Grep, Glob"},{"name":"claude auth","ok":true,"detail":"logged in"},{"name":"codex","ok":true,"info":true,"detail":"not installed"},{"name":"pi","ok":true,"info":true,"detail":"not installed"}]}
```

The plugin looks for the agents again on each `doctor`.

- For each built-in agent:
  - What `--version` says, and a failed check when it fails.
  - How the recipe runs it. The check fails (`doesn't know --ephemeral: upgrade codex`) when the CLI's `--help` for the recipe's command (`codex exec --help`) doesn't list one of its flags, as every ask would then fail. Help that can't be read passes.
  - For Claude and Codex, whether they're logged in. Pi can't say without a provider named.
- A check's name starts with its agent's, so `dffy doctor` folds them into one row per agent, each by its `brief` when it has one: `claude  2.1.329 · read-only: Read, Grep, Glob · logged in`.
- An agent that isn't installed says `not installed` as a note (`"info":true`, `·` in `dffy doctor`). It's a failed check only when the config names it or no agent can answer.
- For a command of yours: whether its program is found.
- For a configured name: whether it names an agent. A name only Fix with agent starts, such as `amp`, passes with a row saying so and where asks go instead (`amp is used for Fix with agent only; asks go to claude`). It fails while it isn't installed.
- The agents are probed side by side, each probe for at most 2 seconds. One that takes longer is killed, and its row fails saying so (`` `claude --version` didn't answer in 2s ``).
- dffy waits 3 seconds for the whole answer.

**Errors.** JSON-RPC errors whose message is for the user:

| Code | Meaning |
|---|---|
| `-32602` | A request it can't take: a missing field, a `cwd` that isn't a directory, or an agent it doesn't know (`no agent named gemini; claude, codex`). |
| `-32800` | Cancelled. |
| `1` | The agent ran and failed: `claude failed: API Error: 529 Overloaded`. |
| `2` | The agent failed and isn't logged in. The message says how to log in, with `claude auth login`, `codex login` or `pi auth login <provider>`. |
| `3` | The agent can't run: it isn't installed. |

`crates/dffy/tests/fixtures/agent-protocol/messages.json` holds a request and its answer for each, and dffy-agent's tests hold the plugin to them. The programs it names are under `@BIN@`, which each side's tests replace with a directory of their own.

**Compatibility.** `initialize` names the protocol both ways, and dffy-agent refuses another, saying which it speaks. Both sides ignore fields they don't know, and a request the plugin doesn't have answers `-32601`.

**What dffy does with it.**

- The viewer asks on `a` or on a comment of yours starting `@<name>` ([viewer.md](viewer.md#asking-an-agent)). It always names a listed agent: for `a`, the one whose answer ends the thread, else the default.
- It writes the answer as a reply in the thread through the comments file's one write. The reply is authored by the agent's `name`, carries the answer's `suggestion`, and has the agent's `mark` as its `label` ([comments.md](comments.md#the-file-version-1)).
- A reply with an author isn't posted until you adopt it. Neither is your comment that asked, a mention or a question, which stays [local only](comments.md#drafts).
- An answer that arrives while a review's post runs is written once the post ends.
- The `initialize` answer isn't cached, so a newly installed agent shows.
- On an ask or Fix with agent, dffy starts the plugin if it isn't running and waits for its `initialize` without holding the viewer, showing `starting dffy-agent…`. Then it sends the ask or starts the handoff it picked. When `initialize` doesn't answer in 2 seconds, the status bar says so, and the ask ends or the thread isn't handed off.
- As the viewer quits, it cancels the running asks, sends `shutdown` and `exit`, waits up to half a second for the plugin to exit, then kills its process group.
- `dffy doctor` lists the plugin's checks.

#### Handoffs

```json
{"name":"claude","program":"/usr/local/bin/claude","args":["{prompt}"]}
{"name":"amp","program":"/usr/local/bin/amp","args":[],"prompt":"clipboard"}
```

A handoff says how to hand a thread to an agent for Fix with agent. dffy starts the agent as you'd start it, in its normal interactive mode with none of a recipe's flags, in a terminal tab at the working tree or in dffy's own terminal.

The plugin lists the agents found on `PATH`, in its order of preference. dffy runs the one the config's `agent` names, else the first, and refuses a name that isn't listed. A command of yours in `agent` takes no thread, so dffy runs the first then.

| Field | Meaning |
|---|---|
| `name` | The agent, lowercase letters, digits, `-` and `_`. It may be one no ask goes to, such as `amp`. |
| `program` | Its program, by an absolute path. |
| `args` | Its arguments, as text. One that's exactly `{prompt}` is replaced by the thread's prompt, one line pointing at the thread ([viewer.md](viewer.md#fixing-with-an-agent)). |
| `prompt` | Optional. `clipboard` for an agent that takes its first message only from you. dffy puts the prompt on the clipboard for you to paste, and `args` has no `{prompt}`. Without it, `args` must have one. |

dffy refuses an `initialize` whose handoffs break these, saying why.

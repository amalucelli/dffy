# Plugins

dffy has three kinds of plugin, each a program on `PATH`: renderer plugins draw files in their rendered form, [review plugins](#review-plugins) connect review comments to a code host, and the [agent plugin](#agent-plugins) asks a coding agent about a review thread.

**Experimental.** The protocols and the settings may change. dffy has no plugin built in, and none is installed by default.

A renderer plugin draws a file in its rendered form, such as a Markdown file with its headings, lists and tables, or a CSV file as rows of cells.

The plugin is a separate program:

1. dffy hands it both sides of a changed file.
2. It hands back each side as blocks of styled lines.
3. dffy diffs those blocks.

The viewer works as on any diff:

- changed blocks are tinted, and an edited paragraph shows the words that changed;
- a block that moved is marked moved (`»`);
- `n`/`N` go between changed blocks, `/` searches the rendered text, and `x`/`X` reveal unchanged blocks.

## Enabling a renderer

1. **Install it.** Two renderers come from this repository:

   | Plugin | Draws |
   |---|---|
   | `dffy-render-markdown` | Markdown files, with ```` ```mermaid ```` fences drawn as diagrams. |
   | `dffy-render-mermaid` | Mermaid files (`.mmd`, `.mermaid`): [flowcharts and sequence diagrams](#the-mermaid-plugin) drawn in text. |

   Both plugins come with each dffy release as separate downloads:

   ```sh
   brew install --cask amalucelli/tap/dffy-render-markdown amalucelli/tap/dffy-render-mermaid     # or:
   curl -fsSL https://raw.githubusercontent.com/amalucelli/dffy/main/install.sh | sh -s -- --plugin markdown --plugin mermaid
   ```

   A plugin must be on `PATH`. [install.md](install.md#plugins) also covers `cargo install --git`, and from a clone `task install:plugins` puts both in `~/.local/bin`.

2. **Map file types to it** in the config file, by extension:

   ```
   render = md:markdown
   ```

   Each pair is `extension:name`, and the name picks the program `dffy-render-<name>` on `PATH`. List several pairs separated by spaces, e.g. `render = md:markdown markdown:markdown` to cover `.markdown` files too, or `render = md:markdown mmd:mermaid mermaid:mermaid` for Mermaid files.

3. **Press `r`** in the viewer on a file of that type, and `r` again to go back to the lines. The status bar says `rendered` while it's on.

   The choice is remembered (`rendered` in the state file) and applies to every file whose type has a renderer; other files keep their lines.

`dffy doctor` lists each configured renderer and whether its program is on `PATH`, with `!` when it isn't.

In the rendered view:

- Lines wrap between words whatever the `wrap` setting says, since renderers leave a paragraph on one line. A word wider than the pane breaks after a hyphen or slash, or wherever it must.
- The gutter numbers each block with the source line it starts on. Hunk headers (`@@ -a,b +c,d @@`) and `⋯ N unchanged lines` count source lines too, blank lines between blocks included.
- A copy (`v`, `y`, or a mouse drag) holds the source of every selected block. The rendered text isn't copied.
- `e` opens the editor at the source line of the block at the top.

When a file can't be rendered, its lines show and the status bar says why:

- no renderer for its type;
- the program isn't on `PATH`;
- the renderer failed;
- the file is binary or too large, or isn't UTF-8 text (it's never sent then).

A diff piped to dffy has only its hunks, so there's nothing to render.

## The Mermaid plugin

`dffy-render-mermaid` draws a Mermaid file's diagram with box characters, so its diff reads as a diagram. A file holds one diagram. The Markdown plugin draws ```` ```mermaid ```` fences the same way.

- **Flowcharts** (`graph`, `flowchart`; `TD`/`TB`, `BT`, `LR`, `RL`):
  - nodes with their labels, in shapes for `[rect]`, `(round)`, `([stadium])`, `((circle))`, `[[subroutine]]`, `[(cylinder)]`, `{diamond}`, `{{hexagon}}`, `>flag]` and `[/lean/]`, and a box for any other;
  - edges `-->`, `---`, `-.->`, `==>`, `--o`, `--x`, `<-->` and `~~~` (spacing only), labeled with `-->|text|` or `-- text -->`, chained (`A --> B --> C`) and grouped with `&`;
  - an edge from a node to itself as `↻` in the node;
  - `subgraph … end` as a titled frame. A frame that would overlap a node outside it or another frame is left out, and a line under the drawing lists its nodes instead.
  - `classDef`, `class`, `style`, `linkStyle` and `click` are skipped. An edge to a subgraph goes to a node of the subgraph's name.
- **Sequence diagrams** (`sequenceDiagram`):
  - a lifeline per `participant` or `actor` (an actor's box has round corners), with `as` aliases;
  - messages `->`, `-->`, `->>`, `-->>`, `-x`, `--x`, `-)`, `--)`, `<<->>` and `<<-->>`, a message to oneself as a loop, and `autonumber`;
  - notes `left of`, `right of` and `over` one or two participants;
  - `loop`, `alt`/`else`, `opt`, `par`/`and`, `critical`/`option` and `break` as titled frames.
  - `activate` and `deactivate` are skipped, as are `box` groups and `rect` highlights.
- **Anything else**, a diagram that doesn't parse, and a diagram past the size limits show as their source lines, a block per line, so their diff is a line diff. The limits: 300 nodes, 600 edges, 100 subgraphs, or 3000 nodes and edge bends in a flowchart; 60 participants or 2000 statements in a sequence diagram; a drawing over a million characters. Node ids are letters, digits and `_`; an id with `-` or `.` doesn't parse.

Blocks:

- A flowchart is one block. When it changes, the old and new drawings show side by side, and words that changed on a line are emphasized. An edge label edited within the room it had changes only its own line. An edit that needs more room, such as a longer node label, moves what's beside or after it, and those lines show as changed too.
- A sequence diagram is a block per statement: the participants, each message, each note, and each frame's top, divider and bottom. An added or edited message shows as that message alone. A message long enough to widen a column also changes every arrow that crosses that column.

Diagrams are fit to the pane's width by wrapping labels and messages. A diagram that's still wider keeps its width, and dffy wraps its lines.

## The renderer protocol, version 1

A renderer is an executable named `dffy-render-<name>`. dffy runs it with no arguments, writes one JSON request to its stdin and closes it, then reads one JSON answer from its stdout.

### The request

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
| `width` | About how many columns a pane has for text; wrap tables and other wide blocks to it. |
| `background` | `dark` or `light`: the terminal's background, for renderers that pick colors. |

### The answer

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

`old` and `new` are each a list of blocks, or `null` when the request's side was `null`. A block is a piece of the document that changes only whole: a heading, a paragraph, a list item, a table row.

| Block field | Meaning |
|---|---|
| `kind` | A word for what it is (`heading`, `paragraph`, `list-item`, `code`, `table-row`...). Any word does; it's part of the block's identity. |
| `source` | `[start, end]`: the lines of that side's text the block comes from, 0-based, `end` not included. It must lie within the side's lines. An empty range (`[n, n]`) is a block drawn from no source line, such as the blank line between two blocks: it gets no line number and is never marked moved. Make that blank line a block of its own rather than the last line of the block before it, or adding a block after that one changes it too. |
| `lines` | The block's lines on screen, each a list of spans. A span is a string, or an object with `text`, a `style` list and, for a link, its `url`. No span may hold a line break. |
| `language` | Optional, for a code block: the language of its text (`rust`, `py`, `sh`). dffy highlights it with its own highlighter when it knows the language. |

Span styles are names:

| Style | Drawn as |
|---|---|
| `bold`, `italic`, `underline`, `dim` | That attribute. |
| `code` | The theme's string color. |
| `link` | Underlined, in the theme's function color. Give the target in the span's `url`. |
| `heading` | Bold; with the span's `level` 1 also underlined, and levels 1 and 2 in the theme's keyword color. |
| `syntax-<role>` | A theme's syntax color: `syntax-keyword`, `syntax-string`, `syntax-comment`, `syntax-function`, `syntax-type`, `syntax-number`, `syntax-constant`, and the other roles in [themes.md](themes.md). |

- Unknown names and fields are ignored.
- Escape sequences aren't interpreted. Text is drawn as text, with control characters shown escaped.
- **Errors:** to fail, answer `{"version": 1, "error": "what went wrong"}`; dffy shows the message whatever the exit status. Any other answer counts only from a renderer that exits with status 0.

### How dffy diffs blocks

- **Identity:** a block's identity is its `kind`, its words with each one's style and link target, and its `source` lines, trailing whitespace aside.
- **Diff:** dffy diffs the two lists of identities with its line diff. A block is unchanged only when its source is; how the rendered text is spaced or wrapped doesn't count.
- **Changed blocks** are removed or added whole.
- **Pairing:** within each run of changed blocks, a removed block pairs with an added block of the same kind whose text is similar enough, by the test that pairs edited lines in a line diff.
- **Word emphasis:** in a pair, a word whose text, style or link target changed is emphasized.
- **Moved:** a changed block whose identity is also among the other side's changed blocks is marked moved.

### Versions

- Both the request and the answer carry `version`.
- dffy refuses an answer of a version it doesn't speak and shows the lines with a note. A renderer should answer a request of a version it doesn't speak with an error.
- The protocol isn't experimental, unlike the review protocol: renderers are written outside this repository, so it doesn't change in place ([design.md](design.md#protocols)). A change that would break existing renderers bumps the version; new optional fields and new style names don't.

## Writing your own

A renderer can be written in any language. It reads all of stdin, renders each side that isn't `null`, and prints the answer.

- Blocks are the unit of change. Make each one as small as one change a reader would notice: a paragraph, an item, a row. A whole table as one block shows a one-cell edit as the whole table replaced.
- Leave paragraphs on one line. dffy wraps them, and word emphasis works best on a paragraph diffed as one line. Wrap only what has its own layout, like tables, to `width`.
- Answer within 500ms. dffy doesn't wait longer.
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

The Markdown plugin in `plugins/markdown` is a fuller example in Rust. It uses pulldown-cmark and draws:

- lists with bullets and numbers;
- quotes with a `│` bar;
- tables with box characters, wrapped to the width;
- links underlined, images as `[image: alt]`, and HTML dim;
- Mermaid fences through the Mermaid plugin's library.

The Mermaid plugin in `plugins/mermaid` lays out graphs itself, with no dependency beyond JSON and character widths.

## Performance and safety

### When renderers run

- Without a `render` setting, nothing about renderers happens.
- With one, dffy looks for a renderer's program on `PATH` only when a file of its type is to be shown rendered. That happens when you press `r`, or when such a file comes up with the rendered view remembered as on.
- The answer is kept for the session.

### The viewer never waits

- A renderer runs in the background; the file's lines show until its rendering arrives.
- Moving to another file, pressing `r` again or quitting stops a renderer still running, with every process it started.

### Timeouts and failures

- A renderer that takes longer than 500ms is killed with every process it started, and the file keeps its lines with a note.
- The same happens when a renderer crashes, prints something that isn't a valid answer, or answers in another protocol version.
- dffy doesn't retry a failed file until it changes or `r` turns rendering on again.

### Caching

Answers are kept in dffy's disk cache (`cache`), so reopening a file doesn't run the renderer again. The key covers:

- the renderer's name, and its program's modification time and size, so replacing the program invalidates its answers;
- both sides' contents;
- the width and the background.

### What can run

- Only programs you installed run, and only `dffy-render-<name>` for a name your config lists.
- Names are lowercase letters, digits, `-` and `_`, so the setting can't point at another path.
- Relative or empty `PATH` entries are skipped, so the repository being viewed can't supply a renderer.
- stderr is discarded, since it would land on the viewer's screen.
- Answers over 64MB are refused.

## Review plugins

A review plugin connects the [review comments](comments.md) to a code host: GitHub, GitLab, Gerrit, or anything else that holds reviews. dffy never touches the network. It runs the plugin in the background, one process for as long as it shows the repository, and talks to it over its stdin and stdout ([the protocol](#the-review-protocol-version-1)). The plugin talks to the host; dffy keeps the review's comments file, sends each request what it needs of the comments, and writes what the plugin sends back. The plugin has no screen of its own: it never prompts or draws, and dffy shows its progress and errors.

### Enabling one

1. **Install it.** `dffy-review-github` comes from this repository; [install.md](install.md#plugins) covers it. It needs [`gh`](https://cli.github.com), logged in.
2. **Name it**, either way:
   - `review = github` in the config file;
   - `--review github` for one run, which wins over the config.
3. **Open a review** with the plugin's command: `dffy pr <number>` for dffy-review-github, or `dffy pr` for the current branch's. [viewer.md](viewer.md#code-host-reviews) covers reviewing it.

`dffy doctor` lists the review plugin named, whether its program is on `PATH`, the command it adds, and the plugin's own checks under it (for dffy-review-github, whether `gh` is installed and logged in), with `!` for each that fails.

### The review command

The plugin names the command: `dffy pr` is dffy-review-github's, and a GitLab plugin could name it `mr`. dffy asks it with `initialize` and adds that command only while a review plugin is set and names one; otherwise there's no such command, `dffy pr` reads `pr` as a revision, and plain `dffy` doesn't ask for the branch's review. The plugin's words go where dffy names what it reviews: the noun ("pull request", "pull requests") in the command's help, the command panel and the picker; the short form and id prefix in labels (`PR #7`); its events in the post dialog. Everything else, from opening a review to posting it, is dffy's.

- dffy refuses a command that isn't lowercase letters, digits and `-` starting with a letter, or that's one of its own subcommands or their aliases (`diff`, `show`, `config`, `doctor`, `help`…). `dffy doctor` says why there's no command.
- dffy starts the plugin for its `initialize` before it reads its arguments when they could name the command (help, or a first word that isn't one of dffy's subcommands and could be a command name), and stops it once answered. Otherwise the viewer asks once its first frame is out, without waiting for the answer, and other runs ask only when they use the plugin. The answer is kept in the disk cache (`cache`), keyed by the plugin program's path, modification time and size: replacing the program asks again, and every `initialize` dffy sends keeps its answer. One that fails or takes over 2 seconds is kept too, as why there's no command: those runs have none, and the error names the plugin when its command was then read as a revision. `dffy doctor` always asks again, and keeps what it's told.

### dffy-review-github

`dffy-review-github` reviews a GitHub pull request in dffy, through `gh` and git. [review-github.md](review-github.md) has how it opens, imports, posts and watches a PR, and what it keeps.

### The review protocol, version 1

The protocol is experimental, and may change while it is. It knows nothing of any host: the plugin names its reviews, its events, labels and groups, and dffy draws them.

**Designed for.** Each message is checked against these before it's added or changed:

- GitHub pull requests, through `gh`: a pending review that a submitting event sends, threads resolved by id.
- GitLab merge requests, through `glab`: draft notes, which a submitting event publishes together; discussions resolved by id; an approval as an event.
- Gerrit changes: a patch set as the session's head, a newer one as `head/moved`; drafts published with a reply; comments ported from older patch sets, placed by their anchors; votes on several labels, which a session names and a `post`'s answer says again once it changed one; a thread resolved by a reply that says so, which the plugin sends from `resolves`.
- A review with no host, kept in the repository: no `url`, no `watch`, and its own `detect` and `list`.

**Program.** A review plugin is an executable named `dffy-review-<name>`, found on `PATH` the way renderers are: names are lowercase letters, digits, `-` and `_`, and relative or empty `PATH` entries are skipped.

**Framing.** [JSON-RPC 2.0](https://www.jsonrpc.org/specification) over the plugin's stdin and stdout, one message a line: a JSON object, its line breaks escaped as JSON escapes them in strings, then `\n`. Lines rather than `Content-Length` headers, because a plugin in any language reads them with its line reader, a shell script included. Requests carry an `id` (dffy's are numbers) and get one answer each, a `result` or an `error`; notifications carry none and get none. stderr isn't shown: when the process exits, its last line of stderr goes in the message about it.

**Lifecycle.**

- **One process a repository.** The viewer starts the plugin on first use, after its first frame: to look for the branch's review, to open one, or for "Open review…". `dffy pr` starts it before the viewer, which takes it over: opening a review costs one process start, however many requests come after.
- **No terminal.** dffy starts it in the root of the working tree, in a session of its own: a prompt for a passphrase or a password fails instead of reaching the screen. The session is its process group too, which dffy kills, with everything in it, when it quits.
- **`initialize` first.** Requests made before its answer wait for it. A plugin that answers it with an error, or writes something that isn't a JSON object before answering, is refused: its requests fail saying why, and it isn't started again.
- **Crashes.** A process that exits otherwise ends the requests it hadn't answered with how it exited, and is started again after 1 second, then 2, 4, 8 and 16 for crashes in a row; one that ran a minute starts the count over, and after 5 in a row it isn't started again. The new one is told again which review dffy shows and whether you're idle. A request it was running isn't sent again: a `post` resumes at the next `P`.
- **Quitting.** dffy sends `shutdown` and `exit`, then kills the process group without waiting. While a `post` runs, it cancels the post and waits for it to end first, up to 30 seconds, after a signal as after the second quit: the plugin stops between drafts, so what it posted until then gets its refs.
- **Short sessions.** dffy also starts the plugin to answer `initialize` alone, when it needs the naming and the cache has none ([The review command](#the-review-command)), and for `doctor` (`dffy doctor`); these run where dffy runs, in a repository or not, and are killed once answered or past their time (2 and 10 seconds).

**Environment.** dffy passes its own environment on, with `DFFY` set to the path of the dffy that started the plugin.

**Messages.**

| Message | Kind | When |
|---|---|---|
| `initialize {protocol, client}` | request | First. `client` is `{"name":"dffy","version":"…"}`. |
| `list` | request | The review command without an id, and "Open review…" in the command panel. |
| `detect` | request | Plain `dffy`, without revisions or paths, on a branch, once the plugin has initialized; the viewer doesn't wait for the answer. |
| `prepare {id, known}` | request | The review command with an id, and a review picked from the list. |
| `pull {id, known}` | request | `p`. |
| `dryRun {id, drafts, resolves}` | request | A moment after your drafts change, and after a `prepare`, `pull` or `post` ends, while there are drafts and no post runs; and as the dialog `P` opens when the last answer is older than the drafts. |
| `post {id, event, labels, summary, drafts, resolves}` | request | `enter` in that dialog, once dffy moved the local review's drafts shown on the review into its comments file, when they go. |
| `doctor` | request | `dffy doctor`; never cached. |
| `shutdown` | request | On quit. The answer, `null`, comes once every request running is answered. |
| `exit` | notification | After `shutdown`: the plugin exits. |
| `$/cancelRequest {id}` | notification | Opening another review or updating cancels a `prepare` or `pull` still running, and the quit or signal that stops a `post` cancels it. dffy also sends it for a request it ends before the plugin's answer: one whose `session` it can't take, whose operations it can't write, or whose words nothing waits for any more. |
| `watch {id}` | notification | To a plugin that watches: the review dffy shows, `null` for none; again to a process started after a crash. |
| `idle/changed {idle}` | notification | To a plugin that watches: `true` after 10 minutes without a key on a review, `false` at the next key. |
| `session {request, session}` | from the plugin | `prepare` or `pull`: the review's [session](#the-session), as soon as its commits are in the clone. dffy opens it at once. |
| `progress {request, progress}` | from the plugin | How far a request got, done of all: `100/312`. The status bar shows it. |
| `comments/apply {request, ops}` | from the plugin | Operations for the review's comments file ([Comments](#comments)), applied until the plugin answers `request`, even after dffy ended it, and ignored after. |
| `head/moved {id, head}` | from the plugin | The review's head on the host, when it isn't the session's, or no longer: dffy says there are new commits, `p` to update. |
| `comments/changed {id}` | from the plugin | The review changed on the host since its comments were imported: dffy says there are new comments, `p` to update. |
| `log {message}` | from the plugin | Words for the status bar, such as why watching failed. |

dffy drops a push about a review it doesn't show. Of `prepare`, `pull` and `post`, one runs at a time, beside `list`, `detect` and `dryRun`: a `prepare` or `pull` replaces the one running, cancelling it, but nothing starts while a `post` runs, and a `post` is refused while a `prepare` or `pull` runs, as both write the review's comments. dffy holds a lock on the review from reading a `post`'s drafts until it ends, so a second viewer's `post` of the same review is refused before it reaches its plugin; a plugin should still refuse a `post` while another runs for the same review, as dffy-review-github does. A plugin should answer requests on threads of its own, so a `dryRun` answers while a `prepare` downloads, and stop a cancelled one where it can, answering it with error `-32800`.

**Answers.** The examples are dffy-review-github's.

| Request | Result |
|---|---|
| `initialize` | [The description](#the-description), with `protocol` and `capabilities`. It should answer at once, from nothing but the plugin itself. |
| `list` | `{"reviews":[{"id":"7","title":"Spell out five","author":"octocat","url":"https://github.com/acme/widgets/pull/7","head_ref":"five","reason":"current_branch"}]}`. Each review has an `id` and an optional `label`, as a session does. `reason` is `current_branch`, `review_requested` or `mine`, which dffy titles and lists first, in that order; a review listed for another reason may carry a `group`, the title it's listed under, groups in the order they first come (`Other` without one). Each review should be listed once. dffy counts your drafts in each review's comments file itself. |
| `detect` | `{"review":{…}}`, the open review of what's checked out, a session, or `{"review":null}`: the review of the branch, or whichever the host ties to the commit, as Gerrit does by its Change-Id. It should answer from what it keeps when it can, since it runs on every start of `dffy`. When it names a review, dffy prepares it and keeps showing what plain `dffy` shows, with the review's comments and status; the whole review (`--merge-base <base>` against the working tree) is a command away. |
| `prepare` | `{}`, once the threads are in, after a `session` notification and `progress` for each page of threads written: threads so far, of all of them. A session answered from what was kept whose head turns out not to be the host's pushes `head/moved`. |
| `pull` | As `prepare`, after fetching the head: a session with the new head. |
| `dryRun` | `{"drafts":[…]}`: a [plan](#a-plan) for each draft, in the order they came. dffy counts them, and what's pending on the host and the resolves waiting, itself. It shouldn't send or write anything. |
| `post` | `{"unposted":[…],"posted":9,"failed":3,"pending":4,"url":"https://github.com/acme/widgets/pull/7#pullrequestreview-2001","last_reviewed_head":"<commit>"}`, after `progress` for each draft: a plan for each draft not posted, all `skip`, then the counts, and the review's `labels` when the post changed a vote. With no drafts and an event that `submits`, it submits what's pending on the host. |
| `doctor` | `{"checks":[{"name":"gh","ok":true,"detail":"gh version 2.62.0"},{"name":"auth","ok":false,"detail":"not logged in"}]}`: what the plugin needs, each with a short `name`, whether it's fine, and a line saying what was found. |

**Errors.** A request that fails answers with JSON-RPC's `error`: `{"code":…,"message":"…","data":…}`. dffy shows the message; for `-32601`, a method the plugin doesn't have, it says the plugin doesn't know the request and may be older than dffy. `-32602` is a request the plugin can't take (an unknown event, an id that isn't one of its own), `-32800` one cancelled. `data`, when there is one, says what was done before the failure, in the result's shape: a `post` stopped midway says what it posted. An error after a `session` notification leaves the session good, since the code is in the clone; it says what's missing, such as the threads, and the viewer stays open. Before one, the review command exits with it.

#### Comments

dffy keeps each review's comments file ([comments.md](comments.md#where-the-file-is)); the plugin never reads or writes it. A request carries only what it needs of the file, as dffy read it when it sent the request.

**The plugin's fields.** A comment from a host may hold fields of the plugin's own beside dffy's, prefixed with its name, such as dffy-review-github's `github_thread`. dffy keeps them, and hands them back wherever a request names that comment: in `known`, in a reply's `in_reply_to`, and in `resolves`.

**`known`**, in `prepare` and `pull`: an entry for each comment that has a `ref`, for the plugin to merge what it imports; empty while dffy has no file for the review.

```json
{"ref":"github:PRRC_kwDOA","anchored":true,"pending":true,"github_thread":"PRRT_kwDOA"}
```

| Field | Meaning |
|---|---|
| `ref` | The comment's. |
| `anchored` | `true` when it has its lines and anchor: an import that can't read its lines again can leave them as they are. |
| `pending` | `true` while it's marked `pending`. |
| others | The plugin's fields. |

An import sends each thread's `resolved` as the host has it, with `host_resolved`. dffy keeps a resolve or reopen made in dffy and not sent through it, even one made after the request went: only `host_resolved` changes then.

**`drafts`**, in `dryRun` and `post`: your [drafts](comments.md#drafts), in the file's order; a dry run's then those of the local review's file shown on the review, its threads on other branches left out. A comment by someone else, such as an agent, is never sent: it would go out under your name.

```json
{"id":"9f3c2a71b04e5d18","path":"src/lib.rs","side":"new","start":{"old":12,"new":12},"end":{"old":null,"new":14},"text":"This can overflow.","anchor":{"before":["…"],"lines":["…"],"after":["…"]},"distance":0}
{"id":"1b7e94c0d25a6f03","path":"src/lib.rs","text":"Same here?","in_reply_to":{"ref":"github:PRRC_kwDOA","github_thread":"PRRT_kwDOA"}}
```

A draft has the comments file's `id`, `path`, `old_path`, `side`, `start_side`, `start`, `end`, `anchor`, `distance`, `text` and `suggestion` that it holds, and:

| Field | Meaning |
|---|---|
| `resolved` | `true` on a thread's first comment resolved before it's posted: it's posted, then resolved. |
| `in_reply_to` | For a reply, its thread's first comment: `{"ref":…}` with the plugin's fields when that's posted, else `{"id":…}`, a draft earlier in the list. |

**`resolves`**, in `dryRun` and `post`: the posted threads resolved or reopened in dffy and not sent, each its first comment's `ref`, the state to send, where it is for words about it, and the plugin's fields: `{"ref":"github:PRRC_kwDOA","resolved":true,"path":"src/lib.rs","side":"new","end":{"old":null,"new":14},"github_thread":"PRRT_kwDOA"}`.

**Out.** What the plugin would change it sends as `comments/apply`: a list of [`dffy comments apply`'s operations](comments.md#operations), which dffy applies to the review's file, all or none, under its lock, as they come, and before it reads anything the request says after them.

- `upsert` by `ref` merges a host's comment field by field, the plugin's fields included; `set` and `rm` name a comment by `id` or `ref`.
- `host_resolved` on a thread's first comment says whether the host has it resolved, as the plugin last saw: an import sets it, and so does sending a resolve. It's how dffy knows what's in `resolves`.
- `gone` names the refs of `known` the host no longer has, once the plugin read every comment; dffy removes them by its rules, keeping a thread that holds your drafts.
- A `set` can't give a comment that has a `ref` another one; clearing it first can.
- When dffy can't apply some, as when a `set` names a draft that's gone, it cancels the request, which ends saying why; what the plugin sends until it answers is still applied, so a post's refs aren't lost. The same holds for a request dffy ended for another reason, and for one cut short by the plugin exiting: what it sent before is in the file, and the next `post` doesn't send those drafts again. Operations for a request the plugin has answered are ignored. The viewer doesn't change the comments while a `post` runs, so what it sent is what the plugin writes back about.

**Posting.** A `post` sends each draft's `ref`, `url` and `pending` as soon as the host has it, so a second `post` never sends it again; and a draft the host refused gets why in its `post_error`, which goes once it's posted. Once a `post` with an event that `submits` ends well, dffy clears every `pending` itself.

**Whole runs.** A message about a whole run that should outlast the status bar goes in the file's `notice` (`{"op":"notice","notice":"…"}`), which dffy shows when it reads the file: why a post stopped, for example.

#### The description

```json
{"protocol":1,"command":"pr","noun":"pull request","nouns":"pull requests","short":"PR","id_prefix":"#","events":[{"id":"pending","key":"p","label":"pending: on GitHub, for you only","done":"review pending","submits":false},{"id":"approve","key":"a","label":"approve","done":"approved"}],"outdated_reply_warning":"GitHub collapses this thread, so a reply there is easy to miss","capabilities":{"watch":true}}
```

| Field | Meaning |
|---|---|
| `protocol` | `1`, the protocol it speaks. dffy refuses another, and says which it speaks. |
| `command` | The subcommand dffy adds: lowercase letters, digits and `-`, starting with a letter. |
| `noun`, `nouns` | What it reviews, singular and plural, up to 40 printable characters each. |
| `short` | Optional: what goes before a review's id in labels, `PR` in `PR #7`. The noun when left out. |
| `id_prefix` | Optional: what goes right before the id, `#` in `PR #7`, up to 8 characters. Nothing when left out. |
| `events` | Optional: the post dialog's choices, in order, 1 to 9. `id` goes to `post`'s `event`, lowercase letters, digits, `_` and `-`, starting with a letter; `key` is one letter or digit that picks it, other than `j`, `k`, `l`, `s` and `v`, which the dialog takes; `label` is shown beside it, and `done` in the status bar once it went (`approved`). `submits` is `false` for an event that leaves the review open on the host, such as a pending review; it's `true` when left out. No ids or keys repeat. Without `events`, there's one: `{"id":"post","key":"p","label":"post","done":"posted"}`. |
| `outdated_reply_warning` | Optional: what dffy says, once, before you reply to a thread the host marks outdated. Nothing when left out. |
| `capabilities` | Optional: `{"watch":true}` for a plugin that watches the review dffy names in `watch`, pushing `head/moved` and `comments/changed`, and pauses while you're idle. dffy sends `watch` and `idle/changed` only to such a plugin, and never asks about a review on a timer of its own. |

dffy refuses a description that breaks these, and `dffy doctor` says why.

#### The session

```json
{"id":"7","title":"Spell out five","url":"https://github.com/acme/widgets/pull/7","base":"<commit>","head":"<commit>","last_reviewed_head":null,"state":"open"}
```

| Field | Meaning |
|---|---|
| `id` | The review, as the plugin's requests take it: a string, one printable line, not starting with `-`. It names the review's comments file too. |
| `label` | Optional: what to show for the id when it isn't the id itself, such as a change number for a long change id. Labels read `<short> <id_prefix><label or id>`. |
| `title`, `url` | The review's. `null` when the host couldn't be reached and nothing was kept from before, or when there's no host to link to. |
| `base`, `head` | Commits in the clone, as full ids: 40 hex digits, or 64 in a SHA-256 repository. dffy shows `<base>...<head>`. |
| `last_reviewed_head` | The head of your last review, a full id, or `null`. The plugin keeps it, and says when a `post` changed it. "Since last review" shows `<last_reviewed_head>..<head>`. |
| `state` | Optional: `open`, `closed` or `merged`, as the host last said. |
| `labels` | Optional: the labels this review is voted on beside the event, up to 9, for a host whose verdict is more than one choice, as a Gerrit change's Code-Review and Verified. They're the review's, so a session names them, not `initialize`: each review may have its own, and your votes on it. Each is `{"name":"Code-Review","values":[{"value":-1,"label":"I would prefer not"},{"value":0,"label":"No score"},{"value":1,"label":"Looks good to me"}],"vote":0}`: a `name` of up to 40 printable characters, 1 to 9 `values` in the order offered, each a whole number with words, and your `vote` on the host now, one of them (the neutral value when you haven't voted). No names or values repeat. The post dialog shows each under the events, starting on your vote, j/k going to it and ←/→ changing it. None when left out. |

dffy refuses a session whose `base`, `head` or `last_reviewed_head` isn't a full commit id, or whose labels break these.

#### A plan

```json
{"id":"9f3c2a71b04e5d18","path":"src/lib.rs","plan":"line","line":13,"side":"new"}
```

| Field | Meaning |
|---|---|
| `id` | The draft's `id` in the comments file. |
| `path` | Its file. |
| `plan` | `line`: on its lines, or a reply in its thread. `file`: a comment on the whole file. `skip`: not posted; it stays a draft. A dry run skips only a draft that can't be posted yet, as what it's on isn't on the host yet: its lines, its file, or its thread's first comment. One the host would refuse for another reason is planned, refused while posting, and gets a `post_error`; `post`'s `unposted` lists it as `skip` too, with the host's reason. The viewer heads a draft planned `file` with `posts as file comment`, and one planned `skip` with its `reason`, or `not in the review yet` without one. |
| `line`, `side` | Where it goes at the review's head, when it has lines: the last line, on the `old` or `new` side. |
| `unsure` | `true` when the plugin chose `line` or `file` without the host's own diff of the file, so the host may disagree: a line comment may be refused, or a file comment may have fit on its lines. Left out otherwise. dffy doesn't read it yet. |
| `reason` | For `skip`: why, as the user reads it. |

For `post`:

- `event` is the `id` of one of the described events, `post` without any; `summary` the review's summary, left out without one. dffy keeps the summary being written in the comments file's `summary`, and removes it once a post with an event that `submits` ends well.
- `labels` holds the votes the dialog has otherwise than the session's `vote`, by name: `{"Code-Review":1}`. A label left on your vote isn't sent, so the vote given before stays; without a change there's no `labels`.
- It posts the `drafts` it's sent, and sends the `resolves`.
- `failed` counts the drafts that weren't posted: skipped by the plan, or refused by the host. Neither holds the event back; the review goes without them.
- `pending` counts the comments your pending review on the host had before the run, which a submitting event sent with the drafts.
- A posted comment is marked `"pending":true` while it's in your pending review, and the mark goes once the review is submitted.
- `url` is the review's link; dffy opens it with `o`.
- `last_reviewed_head`, a full id, when the post recorded a new head as reviewed, as one that posted a comment or applied an event that `submits` should: dffy takes it from there, and the plugin's next session should say the same. dffy never works it out itself. Left out when the post recorded none.
- `labels`, in the session's shape, when the post changed a vote: the review's labels with your votes as the host has them now. dffy takes them as the session's, so the next dialog starts on them and doesn't send a vote that's already given. Left out, the session's stay as they were.
- When it ends, dffy reads the comments file, and its `notice`, if any, shows over dffy's own words for how the post went.

**Compatibility.**

- `initialize` names the protocol both ways; dffy refuses a plugin of another, and dffy-review-github one of another dffy, each saying which it speaks.
- Both sides ignore fields and notifications they don't know. A request a plugin doesn't have answers `-32601`.
- A plugin that can't initialize adds no command and isn't asked for the branch's review, and `dffy doctor` says so; one without `doctor` shows no checks of its own.
- While the protocol is experimental, it changes in place, without a new version number, and dffy and the plugins from this repository change with it.

## Agent plugins

The agent plugin, `dffy-agent`, asks a coding agent you already run about a review thread: Claude Code (`claude`), Codex (`codex`), Pi (`pi`), or a command of yours. It hands back the answer and, when the answer proposes a change, its suggestion, for dffy to put in the thread as a reply authored by the agent. The agent can read and search, but not write, run arbitrary commands or reach the network, because of how it's run, its recipe, not because the prompt asks it to. The plugin writes no file, and neither does the agent.

`dffy-agent` comes from this repository; [install.md](install.md#plugins) covers it. It needs `claude`, `codex` or `pi` on `PATH`, logged in, and nothing else: no SDK, no Node.

### The agents and their recipes

The recipes target the current release of each CLI. An agent is available when its program is on `PATH`; no version is checked, and a CLI release that breaks a recipe gets the recipe fixed in the plugin. `--version` runs only for `doctor`, to show what it says. Every recipe gets the prompt on stdin, never in its arguments, and runs in the working tree.

- **Claude**: `claude -p --verbose --output-format stream-json --tools "Read,Grep,Glob" --disallowedTools "mcp__*" --strict-mcp-config --setting-sources user --no-session-persistence --permission-mode dontAsk`.
  - It can read and search files, and nothing else: `--tools` leaves only Read, Grep and Glob of the built-in tools, so no edits, no shell and no web. MCP servers' tools are denied apart, and `--strict-mcp-config` loads no server. `--setting-sources user` keeps the reviewed repository's settings, and so its hooks, from loading. Nothing is saved as a session.
  - The answer is the final `result` event; one with `is_error` is a failure, its `result` the reason.
  - Replies carry no mark.
- **Codex**: `codex exec --json --sandbox read-only --ephemeral -c web_search="disabled" -C <worktree>`.
  - It can read and search files, and may run commands to do it, inside the read-only sandbox, which the operating system holds to no writes and no network. Web search is off. Nothing is saved as a session. Replies are marked `read-only sandbox`.
  - Codex runs with your own Codex setup: your MCP servers, hooks, profiles and trusted project config load as in your normal use. MCP servers run outside its sandbox.
  - The answer is the last completed item of type `agent_message`; `turn.failed` is a failure, and so is an `error` event when no answer came.
- **Pi**: `pi --mode json --no-session --tools read,grep,find,ls --no-mcp --no-approve --offline`.
  - It can read and search files, and nothing else: `--tools` leaves only the `read`, `grep`, `find` and `ls` tools, so no edits and no shell. `--no-mcp` connects no MCP server, `--no-approve` ignores the reviewed repository's trust-gated `.pi` config, extensions and skills, and `--offline` turns off Pi's own network use; the model's provider is still reached. Nothing is saved as a session. Replies are marked `read-only tools`.
  - Pi has no sandbox: the limit is the tools it's given, not the operating system.
  - With `--offline`, Pi doesn't download the programs its `grep` and `find` tools run, so they work only with `rg` and `fd` installed.
  - The answer is the last assistant message's text. Pi retries a failed message itself, so the last one decides: one that stopped on an error, an abort or the length limit is a failure, its `errorMessage` the reason. Pi exits with status 0 either way.
  - Pi's own login check (`pi auth check`) needs a provider named, which the recipe leaves to your Pi settings, so the plugin doesn't run it. A run that can't start because no model has a key says `No API key found` or `No models available` on stderr, and that's taken as not logged in.
- **A command of yours**: a configured value with a space, its first word a name on `PATH` or an absolute path, the rest its arguments, split at spaces with no shell. Its stdout is the reply, and a non-zero exit a failure. Replies are marked `custom`. Nothing is enforced: the command's flags are yours.

A failure's reason, when the agent's output gives none, is its stderr's first line that starts with `error:`, as a usage error's does, else its last line. When a run of Claude or Codex fails, the plugin asks `claude auth status` or `codex login status`; exit status 1 means it isn't logged in, and the error says how to log in. It runs no login flow.

Amp isn't a built-in agent: it has no read-only mode an ask could rely on, as it runs tools without asking and a repository's `.amp/settings.json` and plugins override restrictions. You can still name it as a command of yours (`agent = amp …`), at your own risk, and Fix with agent can hand it a thread, since it starts an agent in its normal interactive mode rather than read-only.

**What stays possible.**

- Every agent can read files outside the working tree, such as `~/.ssh`: no recipe limits reading to the repository.
- The prompt carries the reviewed code, so it can steer what the agent reads and says. With no writes and no network, what it read leaves only in the answer: a reply shown in dffy, on your machine, never posted to a code host unless you adopt it, and a suggestion only you apply.
- Codex's MCP servers are the exception: the model can call their tools, and they run outside the sandbox, so a server that writes or reaches the network is a way out for an injected prompt.
- dffy's own MCP server is reachable that way when it's one of your Codex servers, and so is the viewer behind it. Its `reply_to_thread` writes a reply by the client, never posted until you adopt it. Its `apply_suggestion`, which writes files, is refused while the viewer asks an agent, as is the control API's `comments/applySuggestion`; `s` in the viewer still applies.
- Your own Claude settings and hooks, in your home directory, load with the user settings.
- Pi's extensions in your home directory (`~/.pi/agent`) load, and run inside Pi with your permissions: one could register tools that write or reach the network, or turn tools back on.
- The agent's own instruction files in the repository, such as `AGENTS.md`, are read as text.

The programs are found in absolute `PATH` entries only, so the reviewed repository can't supply one, and `--version` and the login checks run in `/`.

### The agent protocol, version 1

The protocol is experimental, and changes in place as the review protocol does.

**Program.** The executable `dffy-agent`, found on `PATH` as review plugins are.

**Framing.** As [the review protocol's](#the-review-protocol-version-1): JSON-RPC 2.0 on stdin and stdout, one message a line.

**Lifecycle.**

- **dffy's side.** dffy starts the plugin only when it's needed, on the first ask or for `dffy doctor`, never as it starts, so `initialize` never delays its first frame; it keeps the plugin while it runs.
- **The plugin's side.** Requests run side by side. Each agent runs in a process group of its own, which a cancel kills with everything the agent started; `shutdown`, `exit` and the end of stdin kill every ask still running, as killing the plugin's group doesn't reach them. A plugin that crashes kills none: its agents run on until they next write their output, which has no reader then and fails, or until they end. dffy starts the plugin again, and the asks it was running end as failed.

**Messages.**

| Message | Kind | When |
|---|---|---|
| `initialize {protocol, client, agent}` | request | First. dffy passes the config's `agent` value as it is, unread, and leaves `agent` out without one: a name picks the default agent, a value with a space is a command of yours. |
| `ask {agent, prompt, cwd}` | request | An ask on a thread. `agent` is a listed agent's `name`; `cwd` the working tree, an absolute path. `prompt` is the thread as a prompt ([comments.md](comments.md#as-a-prompt-dffy-comments-prompt)), complete: the plugin sends it to the agent as it is, adding nothing, so the prompt itself ends with the ask for a ```` ```suggestion ```` block. |
| `doctor` | request | `dffy doctor`. |
| `shutdown` | request | On quit: every ask still running is killed, and it answers `null`. |
| `exit` | notification | After `shutdown`: the plugin exits. |
| `$/cancelRequest {id}` | notification | Cancels an ask: its agent is killed, and the ask answers `-32800`. |
| `progress {request, progress}` | from the plugin | What the agent is doing: `starting…`, `thinking…`, `writing…`, `running a command…`, `reading…`, `searching…`. Said once each time it changes. |

**Answers.**

| Request | Result |
|---|---|
| `initialize` | `{"protocol":1,"agents":[{"name":"claude","default":true,"mark":null},{"name":"codex","default":false,"mark":"read-only sandbox"}],"capabilities":{}}`. `agents` lists only the agents found. `name` is `claude`, `codex`, `pi` or `custom`; one is the `default`: the command of yours when there is one, else the agent the config names, else the first. `mark` is how its replies are labeled, `null` for none. |
| `ask` | `{"text":"Yes: the sum overflows past u32::MAX.","suggestion":"x.checked_add(y)?"}`. The answer's last ```` ```suggestion ```` fenced block, closed by a fence at least as long as the one that opened it, is taken out of `text` and becomes `suggestion`: its lines, without a final line break, as [the comments file's](comments.md#the-file-version-1) `suggestion`. A fence inside another block opens none. Without one there's no `suggestion`; an empty block is an empty one, which removes the lines. |
| `doctor` | `{"checks":[{"name":"claude","ok":true,"detail":"2.1.329 (Claude Code)"},{"name":"claude recipe","ok":true,"detail":"read-only: Read, Grep and Glob only, MCP tools denied, user settings only"},{"name":"claude auth","ok":true,"detail":"logged in"},{"name":"codex","ok":true,"detail":"not installed"},{"name":"pi","ok":true,"detail":"not installed"}]}`, looked for again. For each built-in agent: what `--version` says, a failed check when it fails, and how the recipe runs it; for Claude and Codex also whether they're logged in, which Pi can't say without a provider named. One that isn't installed says `not installed`, a failed check only when the config names it or no agent can answer. For a command of yours: whether its program is found. And whether a configured name names an agent. |

**Errors.** As JSON-RPC errors whose message is for the user:

| Code | Meaning |
|---|---|
| `-32602` | A request it can't take: a missing field, a `cwd` that isn't a directory, or an agent it doesn't know (`no agent named gemini; claude, codex`). |
| `-32800` | Cancelled. |
| `1` | The agent ran and failed: `claude failed: API Error: 529 Overloaded`. |
| `2` | The agent failed and isn't logged in; the message says how to log in, with `claude auth login`, `codex login` or `pi auth login <provider>`. |
| `3` | The agent can't run: it isn't installed. |

`crates/dffy/tests/fixtures/agent-protocol/messages.json` holds a request and its answer for each, and dffy-agent's tests hold the plugin to them.

**Compatibility.** `initialize` names the protocol both ways, and dffy-agent refuses another, saying which it speaks. Both sides ignore fields they don't know, and a request the plugin doesn't have answers `-32601`.

**What dffy does with it.** The viewer asks on `a` or a comment of yours starting `@<name>` ([viewer.md](viewer.md#asking-an-agent)), always naming a listed agent, the default for `a`. It writes an answer as a reply in the thread through the comments file's one write: authored by the agent's `name`, with the answer's `suggestion`, and the agent's `mark` as its `label` ([comments.md](comments.md#the-file-version-1)); a reply with an author isn't posted until you adopt it, and neither is your comment that asked, a mention, a question or a follow-up, which is kept [local only](comments.md#drafts). An answer that comes while a review's post runs is written once the post ends. The `initialize` answer isn't cached, so an agent installed since shows. As the viewer quits it cancels the asks running, sends `shutdown` and `exit`, waits up to half a second for the plugin to exit, then kills its process group. `dffy doctor` lists the plugin's checks.

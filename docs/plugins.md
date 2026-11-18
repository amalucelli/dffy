# Plugins

dffy has two kinds of plugin, each a program on `PATH`: renderer plugins draw files in their rendered form, and [review plugins](#review-plugins) connect review comments to a code host.

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
- A change that would break existing renderers bumps the version; new optional fields and new style names don't.

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

- A renderer runs on dffy's worker threads; the file's lines show until its rendering arrives.
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

A review plugin connects the [review comments](comments.md) to a code host: GitHub, GitLab, Gerrit, or anything else that holds reviews. dffy never touches the network. It runs the plugin in the background and reads its answers as JSON lines; the plugin talks to the host and writes the comments file with `dffy comments apply`. The plugin has no screen of its own: it never prompts or draws, and dffy shows its progress and errors.

### Enabling one

1. **Install it.** `dffy-review-github` comes from this repository; [install.md](install.md#plugins) covers it. It needs [`gh`](https://cli.github.com), logged in.
2. **Name it**, either way:
   - `review = github` in the config file;
   - `--review github` for one run, which wins over the config.
3. **Open a review** with the plugin's command: `dffy pr <number>` for dffy-review-github, or `dffy pr` for the current branch's. [viewer.md](viewer.md#code-host-reviews) covers reviewing it.

`dffy doctor` lists the review plugin named, whether its program is on `PATH`, the command it adds, and the plugin's own checks under it (for dffy-review-github, whether `gh` is installed and logged in), with `!` for each that fails.

### The review command

The plugin names the command: `dffy pr` is dffy-review-github's, and a GitLab plugin could name it `mr`. dffy asks it with `describe` and adds that command only while a review plugin is set and describes one; otherwise there's no such command, `dffy pr` reads `pr` as a revision, and plain `dffy` doesn't ask for the branch's review. The plugin's words go where dffy names what it reviews: the noun ("pull request", "pull requests") in the command's help, the command panel and the picker; the short form and id prefix in labels (`PR #7`); its events in the post dialog. Everything else, from opening a review to posting it, is dffy's.

- dffy refuses a command that isn't lowercase letters, digits and `-` starting with a letter, or that's one of its own subcommands or their aliases (`diff`, `show`, `config`, `doctor`, `help`…). `dffy doctor` says why there's no command.
- dffy runs `describe` before it reads its arguments, so the answer is kept in the disk cache (`cache`), keyed by the plugin program's path, modification time and size: replacing the program asks again. A `describe` that fails or takes over 2 seconds isn't kept; that run has no command, and the next run asks again.

### dffy-review-github

It reviews a GitHub pull request in dffy. Run it in a clone whose `origin` is on GitHub.

- **`gh`.** It reaches GitHub only through the [GitHub CLI](https://cli.github.com), logged in. It names the host as `GH_HOST`, else github.com, and its `doctor` checks the login there. Without `gh` on `PATH`, a verb that asks GitHub fails with `dffy-review-github needs the GitHub CLI (gh): https://cli.github.com`; logged out, or with a token GitHub refuses, with ``gh isn't logged in to github.com; run `gh auth login` ``. Neither costs a call of its own: the first request tells. A `prepare` that has to ask GitHub then prints no session; one answered from its session still opens, with the error after it.

- **Storage.** It keeps a comments file per PR, drafts included, the PR's session, and GitHub's hunks of the PR at the session's head, under `$XDG_DATA_HOME/dffy-review-github/<repository>/<pr>`. It names that file in the session and doesn't read `DFFY_COMMENTS`.
- **Opening.** The code comes only through `git fetch`, as the ref `refs/dffy/pull/<pr>`.
  - A PR prepared before, whose commits are still in the clone, answers from its session before asking GitHub anything. Its threads are downloaded after that.
  - The session's head stays the one dffy shows until a `pull`, even when the PR has moved on. `check` says when it has.
  - When GitHub can't be reached, the code still opens on the fetched head, with the base the session had or the default branch's tip, and the threads are left as they were.
- **Threads.** Threads are merged into the file by `ref`, and drafts are never touched. A thread you resolved or reopened in dffy keeps your state until it's posted. Each comment gets its `url`, and a thread GitHub marks outdated gets `outdated`. A thread's lines and anchor are those it was written on, read from its original commit (at that commit's merge base with the base, for the old side), outdated or not: GitHub's own placement on the current head is ignored, and dffy places the thread by its anchor's text.
  - An original commit a force-push left on no ref isn't brought by the head's fetch. After the last page, the original commits the clone doesn't have are fetched by oid in one `git fetch`, onto no ref, and their threads are placed then. Until then, and for good when GitHub no longer has the commit, the thread is a file comment with `"github_unplaced":"original commit not available"`. A commit GitHub lacks fails the whole fetch, so it's fetched again without the one git names.
  - Once placed, a thread keeps its lines even when its commit later goes from the clone, so the commit is never fetched again for it. One GitHub has dropped is asked for again on each `prepare` or `pull`, after the session line.
- **Posting.**
  - Each comment is its own request, paced to stay under GitHub's limit, and its `ref` is written back as soon as it's posted, so posting again never sends one twice.
  - Comments are posted against the session's head, even when the PR has moved on.
  - Before sending, each draft is planned from git and the hunks `prepare` or `pull` kept, without asking GitHub; `post --dry-run` prints the same plan without sending anything:
    - A draft goes on the lines its anchor has in the file at that head (at its merge base with the base, for the old side), not on the line numbers it was written at: a draft written on the working tree, with lines added above it, moves to where its lines are. One whose lines aren't at the head is skipped: `lines not in the PR head (unpushed changes?)`.
    - A draft on a file the PR doesn't change is skipped: `file not in the PR`.
    - A line comment goes on its lines only when its first and last line, where they are at the head, are in one hunk of GitHub's diff of the PR; else it becomes a file comment that quotes the lines, since GitHub refuses it on them. `prepare` and `pull` ask GitHub for the PR's files beside its threads and keep their hunks for the head the session is on.
    - Where GitHub's hunks aren't kept for the session's head, or GitHub left a file's patch out (too large, or past the 3000 files it lists), git's diff of the PR (`base...head`, 3 lines of context, two hunks one line apart joined into one as GitHub joins them) decides instead, and the draft line says `"unsure":true`: git can still draw a hunk's edges where GitHub doesn't. The draft's `distance` was measured on the diff it was written on, so it's used only when git can't diff the PR either.
    - A reply goes in its thread when the thread is on GitHub, or its first comment is planned to post before it; else it's skipped: `its thread isn't posted`.
  - Only your drafts are posted: a comment without a `ref` whose `author` isn't empty, such as an agent's, is left alone, neither planned nor counted.
  - A skipped draft stays a draft, with the reason in the file's `notice`, and the review is still submitted with the event: dffy showed the skips before you sent.
  - A draft GitHub refuses while posting stays a draft too, but then a submitting event (`comment`, `approve`, `request_changes`) isn't submitted: the review stays pending, and the final line says `"held":true,"held_message":"review still pending"`. `post --apply-event` submits it without them. `pending` is never held: it leaves the review pending as asked.
- **Detecting.** `detect` keeps the PR's branch in its session, and answers from the newest session kept for the branch checked out without asking GitHub. Else it asks for the repository's open PR from that branch, in one query.
- **Last reviewed head.** A `post` that finishes, having posted a comment or submitted the review, records the session's head, the one it posted against, as `last_reviewed_head`; `pull` leaves it. "Since last review" compares with it, however many updates came since.
- **`dffy-review-github prefetch`** is yours to run, not dffy's. It fetches the heads of the PRs `list` shows, so preparing one later waits only for its threads, and prints `{"fetched":[7,9]}`.
- **Its words.** It describes `dffy pr`, "pull request", `PR #7` (its ids are PR numbers, and its labels left out), the events `p` pending (`"submits":false`), `c` comment, `a` approve and `r` request changes, and the warning that GitHub collapses outdated threads, so a reply there is easy to miss. Its `list` groups by `current_branch`, `review_requested` and `mine`. `check` asks git only (`git ls-remote`), never GitHub's API.

### The review protocol, version 1

The protocol is experimental, and may change while it is. It knows nothing of any host: the plugin names its reviews, its events and its groups, and dffy draws them.

**Program.** A review plugin is an executable named `dffy-review-<name>`, found on `PATH` the way renderers are: names are lowercase letters, digits, `-` and `_`, and relative or empty `PATH` entries are skipped.

**When dffy runs each verb.**

| Verb | When |
|---|---|
| `describe` | Before dffy reads its arguments, when the disk cache has no answer for the program as installed ([The review command](#the-review-command)) |
| `doctor` | `dffy doctor`; never cached |
| `list` | The review command without an id, and "Open review…" in the command panel |
| `detect` | Plain `dffy`, without revisions or paths, on a branch, as the viewer starts, when the plugin describes itself; it doesn't wait for the answer |
| `prepare <id>` | The review command with an id, and a review picked from the list |
| `check <id>` | Every 60 seconds while a review is open, unless `prepare` or `pull` is running |
| `pull <id>` | `p` |
| `post <id> --dry-run` | Opening the dialog `P` opens |
| `post <id> --event <event>` | `enter` in that dialog, once dffy moved the local review's drafts shown on the review into the comments file |
| `post <id> --apply-event --event <event>` | `s` in the prompt after a post that held a submitting event back |

**Running.**

- dffy runs the program in the root of the working tree, with stdin closed, in a session of its own, so it has no terminal: a prompt for a passphrase or a password fails instead of reaching the screen. The session is its process group too, which dffy kills, with everything in it, when it quits.
- stderr isn't shown. When the plugin fails without an error line, its last line of stderr goes in the message.
- `describe` and `doctor` are the exceptions: they run where dffy runs, in a repository or not, with stderr discarded, and are killed past their time (2 and 10 seconds).
- At most one `prepare`, `pull` or `post` runs at a time, beside `check` on its own timer, `list`, `detect` and `post --dry-run`. Opening another review or updating stops a `prepare` or `pull` still running; none of the three starts while `post` runs.
- `post` can't be cancelled midway, but a second run resumes it.

**Environment.** dffy passes its own environment on, with these set:

| Variable | Value |
|---|---|
| `DFFY_REVIEW_PROTOCOL` | `1`. A plugin that speaks another protocol fails with exit status 2. Unset when you run the plugin yourself. |
| `DFFY_COMMENTS` | For `check`, `pull` and `post`: the comments file the viewer shows, the session's, as an absolute path. Unset for `describe`, `doctor`, `list`, `detect` and `prepare`. |

**Output.** The plugin prints one JSON object per line on stdout, and each carries `"version": 1`. dffy reads each line as it comes: the viewer opens on the first session line, and progress shows in the status bar.

The examples are dffy-review-github's.

| Verb | Lines |
|---|---|
| `describe` | One line, [the description](#the-description). It runs anywhere, not only in a repository, and should answer at once, from nothing but the plugin itself. |
| `doctor` | One line: `{"checks":[{"name":"gh","ok":true,"detail":"gh version 2.62.0"},{"name":"auth","ok":false,"detail":"not logged in"}]}`, what the plugin needs, each with a short `name`, whether it's fine, and a line saying what was found. It exits 0 whatever the checks find; dffy waits up to 10 seconds for it. |
| `list` | One line: `{"reviews":[{"id":"7","title":"Spell out five","author":"octocat","url":"https://github.com/acme/widgets/pull/7","head_ref":"five","reason":"current_branch"}]}`. Each review has an `id` and an optional `label`, as in a session line. `reason` is `current_branch`, `review_requested` or `mine`, which dffy titles and lists first, in that order; a review listed for another reason may carry a `group`, the title it's listed under, groups in the order they first come (`Other` without one). Each review should be listed once. |
| `detect` | One line: `{"review":{…}}`, the open review whose branch is the one checked out, with the fields of a session line, or `{"review":null}`. It should answer from what it keeps when it can, since it runs on every start of `dffy`. When it names a review, dffy runs `prepare` on it and keeps showing what plain `dffy` shows, with the review's comments file and status; the whole review (`--merge-base <base>` against the working tree) is a command away. |
| `prepare <id>` | First, as soon as the review's commits are in the clone, a session line. Then `{"progress":"100/312"}` as each page of threads is written: threads written so far, of all of them. dffy doesn't wait for it to exit. |
| `check <id>` | One line: `{"head":"<commit>","moved":true}`, the review's head on the host and whether it differs from the session's. It should be cheap; dffy runs it every 60 seconds. |
| `pull <id>` | Fetches the head and the threads. Then, as for `prepare`, a session line with the new head, followed by progress lines. |
| `post <id> --dry-run` | One draft line per draft, in the comments file's order, saying what `post` would do with it, then `{"planned":{"line":9,"file":1,"skip":2}}`. It shouldn't send or write anything. |
| `post <id> --event <event> [--summary-file <file>]` | `{"progress":"3/12"}` after each draft, then a draft line for each draft not posted, then `{"posted":9,"failed":3,"held":true,"held_message":"review still pending","url":"https://github.com/acme/widgets/pull/7#pullrequestreview-2001"}`. |
| `post <id> --apply-event --event <event> [--summary-file <file>]` | Applies a submitting event a `post` held back, posting no drafts, then `{"posted":0,"failed":0,"url":"…"}`. |

#### The description

```json
{"command":"pr","noun":"pull request","nouns":"pull requests","short":"PR","id_prefix":"#","events":[{"id":"pending","key":"p","label":"pending: on GitHub, for you only","done":"review pending","submits":false},{"id":"approve","key":"a","label":"approve","done":"approved"}],"outdated_reply_warning":"GitHub collapses this thread, so a reply there is easy to miss","version":1}
```

| Field | Meaning |
|---|---|
| `command` | The subcommand dffy adds: lowercase letters, digits and `-`, starting with a letter. |
| `noun`, `nouns` | What it reviews, singular and plural, up to 40 printable characters each. |
| `short` | Optional: what goes before a review's id in labels, `PR` in `PR #7`. The noun when left out. |
| `id_prefix` | Optional: what goes right before the id, `#` in `PR #7`, up to 8 characters. Nothing when left out. |
| `events` | Optional: the post dialog's choices, in order, 1 to 9. `id` goes to `post --event`, lowercase letters, digits, `_` and `-`, starting with a letter; `key` is one letter or digit that picks it, other than `j`, `k`, `s` and `v`, which the dialog takes; `label` is shown beside it, and `done` in the status bar once it went (`approved`). `submits` is `false` for an event that leaves the review open on the host, such as a pending review, which drafts the host refuses never hold back; it's `true` when left out. No ids or keys repeat. Without `events`, there's one: `{"id":"post","key":"p","label":"post","done":"posted"}`. |
| `outdated_reply_warning` | Optional: what dffy says, once, before you reply to a thread the host marks outdated. Nothing when left out. |

dffy refuses a description that breaks these, and `dffy doctor` says why.

A draft line:

```json
{"draft":{"id":"9f3c2a71b04e5d18","path":"src/lib.rs","plan":"line","line":13,"side":"new"},"version":1}
```

| Field | Meaning |
|---|---|
| `id` | The draft's `id` in the comments file. |
| `path` | Its file. |
| `plan` | `line`: on its lines, or a reply in its thread. `file`: a comment on the whole file. `skip`: not posted; it stays a draft. A `post` prints draft lines only for drafts it didn't post, all `skip`. |
| `line`, `side` | Where it goes at the review's head, when it has lines: the last line, on the `old` or `new` side. |
| `unsure` | `true` when the plugin chose `line` or `file` without the host's own diff of the file, so the host may disagree: a line comment may be refused, or a file comment may have fit on its lines. Left out otherwise. dffy doesn't read it yet. |
| `reason` | For `skip`: why, as the user reads it. |

A session line:

```json
{"session":{"id":"7","title":"Spell out five","url":"https://github.com/acme/widgets/pull/7","base":"<commit>","head":"<commit>","last_reviewed_head":null,"comments":"/home/ada/.local/share/dffy-review-github/github.com_acme_widgets/7/comments.json"},"version":1}
```

| Field | Meaning |
|---|---|
| `id` | The review, as the plugin's verbs take it: a string, one printable line, not starting with `-`. |
| `label` | Optional: what to show for the id when it isn't the id itself, such as a change number for a long change id. Labels read `<short> <id_prefix><label or id>`. |
| `title`, `url` | The review's. `null` when the host couldn't be reached and nothing was kept from before. |
| `base`, `head` | Commits in the clone, as full ids. dffy shows `<base>...<head>`. |
| `last_reviewed_head` | The head of your last review posted through the plugin, or `null`. "Since last review" shows `<last_reviewed_head>..<head>`. |
| `comments` | The comments file, as an absolute path. |

dffy refuses a session whose `base`, `head` or `last_reviewed_head` isn't a commit id, or whose `comments` isn't absolute.

For `post`:

- `<event>` is the `id` of one of the described events, `post` without any.
- It posts drafts only: comments without a `ref` whose `author` is empty. A comment without a `ref` by someone else, such as an agent, would go out under your name, so `post` and its dry run leave it alone, and don't count it.
- `--summary-file` names a file holding the review's summary. dffy writes it for the run and removes it after; without a summary, there's no flag.
- `failed` counts the drafts that weren't posted.
- `held` is `true` when the event wasn't applied, because the host refused drafts (or resolves) and the plugin holds a submitting event back without them; drafts skipped by the plan shouldn't hold it. `held_message` is optional: the plugin's words for where that leaves the review, which dffy shows (`review still pending`; `review not sent` without one). Both are left out otherwise. When `held` is `true` and `failed` isn't 0, dffy asks whether to apply the event without them (`--apply-event`), go to them, or leave the review as it is.
- `url` is the review's link; dffy opens it with `o`.
- After a `post` that exits 0 and posted a comment, or applied an event that `submits`, dffy takes the session's head as `last_reviewed_head`, and the plugin's next session line should say the same.
- When the run ends, dffy reads the comments file, and its `notice`, if any, shows over dffy's own words for how the post went.

**Errors.** A failure ends with `{"error":"…"}` as the last line and a non-zero exit status: 2 for a verb, event or argument the plugin doesn't take, or a protocol it doesn't speak, and 1 for anything else. dffy shows the error line's message, over the exit status. An error after a session line leaves the session good, since the code is in the clone; it says what's missing, such as the threads, and the viewer stays open. Before a session line, the review command exits with it.

**Writing.** A plugin changes the comments file only through `dffy comments apply` ([comments.md](comments.md)), which holds the file's lock. A message that should outlast the status bar goes in the file's `notice`, which dffy shows when it reads the file: why a comment wasn't posted, for example.

**Compatibility.**

- A plugin fails with exit status 2, or an error line, for a verb or event it doesn't support.
- dffy stops a run at the first line that names another `version`, and says which it speaks.
- Both sides ignore fields they don't know.
- A plugin without `describe` adds no command and isn't asked for the branch's review, and `dffy doctor` says so; one without `doctor` shows no checks of its own.
- While the protocol is experimental, it changes in place, without a new version number, and dffy and the plugins from this repository change with it.

# Architecture

This page maps dffy's crates and modules and how data moves through them, for contributors and coding agents. [design.md](design.md) has the rules a change follows: where code goes, ownership, protocols, performance and testing.

dffy's priorities, in order, are performance, usability and developer experience. The reference workload is the Linux kernel repository: 97,000 files, of which `v6.0..v6.1` changes 13,168.

## Crates

- `dffy-core` is the library that decides what a diff means. It's versioned on its own and not published to crates.io. Its API is 0.x and unstable.
- `dffy` is the binary that shows it in a terminal. It has no supported API.
- `xtask` is the benchmark tooling.
- `plugins/` holds the plugins: two renderers (`markdown`, `mermaid`), the GitHub review plugin (`review-github`) and the agent plugin (`agent`). Each is a program of its own and is never linked into dffy.

### `dffy-core`

The engine: git access, the line and intra-line diff, the syntax diff, highlighting, patch parsing, the document a frontend shows (files, rows, line states, positions, navigation, search), and annotations placed on its files' lines. It has no terminal code and doesn't assume which process hosts it.

Core decides and the host draws. Core decides which lines changed, what each line is (modified, moved, reformatted), the rows of each arrangement, where a position is, where navigation goes and what a search matches. The host supplies the executor, text measurement (widths, wrapping), colors and drawing.

Core has no thread pool or event loop. The host runs the work `Document::missing` names on its own threads. A few calls fork scoped threads for one job and join them before returning: parsing two sides at once, the status scan, rename scoring. The watcher and the cache's trimming run their own threads.

A module imports only from its own layer and the layers below it:

| Layer | Modules | Built on |
|---|---|---|
| 0 | `diff`, `patch`, `view`, `conflict` | memchr and rustc-hash. No git, no tree-sitter, no I/O or threads. |
| 1 | `highlight`, `structural` | tree-sitter |
| 2 | `git`, `watch`, `cache` | gix and the filesystem |
| 3 | `document`, `annotation`, `navigate` | Everything below |

Layer 1 is the exception. `highlight` and `structural` store results through `cache` and name blobs by `git::ObjectId`, and the grammar registry hashes grammar identities with gix. `suggest` imports nothing, and `git` uses it.

| Module | What it owns |
|---|---|
| `diff` | The hunk model, and the line diff: histogram with a Myers fallback. |
| `diff::intraline` | Token-level emphasis inside changed lines, and pairing a change block's removed and added lines. It serves hunks and a patch's lines alike. |
| `view` | What a frontend draws, short of how it looks: a hunk's lines and their states (modified, moved, reformatted), headers, rows in the split or unified arrangement or of one side read alone, syntax runs, paint and attributes, hunk headings. No widths, colors or cells. |
| `patch` | Unified diffs read as text (`git diff`, `git log -p`, `diff -u`): the line-at-a-time parser and its events, the kinds of text and combined-diff lines, and the builder that collects events into commits and files. No I/O. The host reads the input and stores what it keeps. |
| `highlight` | tree-sitter highlighting of the requested lines, and a file's two sides prepared for it. The grammar registry (`Registry`) is a value the host builds and passes in, with the 30 bundled grammars as `Registry::builtin`. Matching a file to a language reads only names and extensions. A grammar loads on its language's first parse or query compile. Each grammar has an identity that goes into cache keys. |
| `structural` | The syntax-aware diff: anchoring, per-region matching, the result cache. |
| `conflict` | Merge conflicts in a conflicted file: the marker rule, the regions the markers bound (settled by the index's sides where they read two ways), a choice per region, the text the choices resolve to, and the preview's hunks. |
| `git` | Revision resolution (through gix), streaming change enumeration, content loading. A `Snapshot` is one side of a diff as a whole: a tree, the index or the working tree. `Repo::list_paths` lists its files in git's order, and `Repo::side_at` finds one path in it. `DiffSpec::snapshots` gives a diff's two. `FileChange::unchanged` is one side compared with itself, which `Repo::file_diff` reads once and finds no hunks in. `git::worktree_file` gives the mode of a repository-relative path in the working tree, or `None` where git wouldn't track a file: a missing path or a directory, a path under a directory that is a symbolic link, an absolute path, one with an empty, `.` or `..` component, or one inside `.git` in any case. A path from outside git, such as a comment's, is safe to read and write once it passes. |
| `git::spec` | `git diff`-style arguments resolved to the two sides of a diff. |
| `git::tree` | Tree-to-tree and tree-to-index diffs as ordered merge-joins that skip equal subtrees. |
| `git::status` | The working-tree side: a parallel stat scan of the index in index order, and the untracked walk. |
| `git::rename` | Rename detection after enumeration, following git's diffcore-rename so pairs and scores match `git diff -M`. |
| `git::changes` | A diff's file list kept current for watch mode, with the parsed index kept loaded. |
| `git::checkpoint` | Checkpoints: the working tree as the uncommitted diff lists it, written as a commit under `refs/worktree/dffy/checkpoints/<n>`. Its tree is `HEAD`'s with the status scan's changes and the untracked files. The last 5 are kept per worktree. Taking one never touches the index, `HEAD` or the working tree. Comparing with one is `DiffSpec::Checkpoint`: the status scan against its tree, with untracked files paired to the tree's by path. |
| `git::conflict` | A conflicted file's conflicts as the index (stages 1 to 3) and the working tree have them, why one can't be resolved by region, and writing a resolution. |
| `git::paths` | Two files or folders compared outside git, behind the same `Repo` handle. It walks both trees in step. A worker per core stats and compares files, and results come back in path order. |
| `watch` | The file watcher: inotify and FSEvents backends, coalescing, ignore rules, polling. |
| `cache` | The content-addressed disk cache shared by every dffy process. |
| `document` | A diff as a frontend shows it, one file at a time. `Document` holds every entry (files, and a piped log's commits) with its prepared diff, revealed lines, counts, version, saved position and rendered view. It owns the position and navigation by row, change and file, and the syntax-diff, full-text and rendered toggles. It keeps the choices on a conflicted file's regions, which the diff then previews, evicts the least recently shown diffs, and holds the search's matches. `Document::missing` is the work the rows about to be shown still lack. A move that needs the host's screen returns a `Settle` the host finishes. `File` is one entry's diff: its parts (hunks and gaps of unchanged lines), rows per arrangement or side read, gap reveal, the overview's marks, search, and a rendered view's mapping to its source. `File::prepare` is the per-file work, split per file, hunk and gap so a host runs only what it draws. It reads no files but the cache's and starts no threads. The host fetches content and schedules the work. An entry's `Source::Unchanged` is a file the diff doesn't list, shown whole for what's on it, such as a file with comments: `Document::show_unchanged` adds it after the diff's files, its change is `FileChange::unchanged`, and `Prepare::unchanged` builds it as one gap of every line, always shown. It stays through `Document::replace` until a refreshed list has its path, and then it's that file's entry. |
| `document::blocks` | Rows a host inserts among a file's rows. A block is an id, where it hangs (under the first row, under the row showing a line of one side, or under the last row) and plain text. Blocks are rows of the row model, so `Pos`, movement, `Document::missing`, revealing and the full-text toggle count them, and a position on one stays put when rows change around it. Blocks hung at one place are parted by the blank rows `Arrange::parting` asks for. A block on a hidden line hangs under its gap's separator. Heights come from the host through `Arrange::measure`, asked only for the parts a call touches and kept per block and width until the host calls `Document::remeasure`. `Document::relayout` keeps the position on the line or block row it showed. Search reads block text as a second list of matches placed among the lines' (`Matches::get` goes through both in order), and `Document::step_block` walks blocks across files. What a block shows is the host's. |
| `annotation` | Ranges of a file's lines that something outside the diff holds on to (the viewer's comments are one source). Each is recorded with its side, its lines and the text around them. On a version of the file it lands at its recorded lines when the text still matches, else where its lines and context appear once within 1,000 lines, else it's lost. `AnnotationSet` stamps each path with the generation its annotations last changed at, so a stale placement is detected and a change to one path re-places that path alone. Placement reads only the diff's two sides, so the host runs it off its main thread. `Document::reveal_annotations` then opens the gap lines around each placed one. What an annotation says is the host's. |
| `navigate` | Go-to-definition at one side of a diff, by matching names rather than resolving them. A `Navigator` lists the side's files (for the working tree, the diff's other snapshot with its changes laid over it) and answers a lookup from the asking file, what it includes or `use`s and the diff's nearest files of its language, read from the symbol cache or parsed on demand, the asking file always and first, the rest within 100ms in all (what's left, and a file too slow to parse, go first in the index build), then from the index of every file of a language with a tags query, read through a posting of each name to the files defining it. A lookup can be cancelled between files. An `#include` resolves where a compiler of the asking file looks first: next to the including file, the `include/` of each folder above it, the asking file's architecture's headers (x86's outside `arch/`), then `include/`. Other languages' imports are mapped onto the side's paths by each one's convention (`navigate::imports`), and followed through the imported files' re-exports. The host builds the index a chunk at a time on its own threads, one chunk per symbol cache pack, so a build resumes where it stopped, and a lookup answers from what's built so far and says how far that is. Navigators made with one `Shared` store, both sides of a diff and those made again after a reload, hold each blob's definitions once. Candidates rank by the same file, then included or imported files, then the same folder or crate, then the rest, a member of a type the asking file names first within each, other architectures after the asking file's, re-exports after every definition, and say whether their file changed in the range and whether the diff touches their lines; a lookup lists the best 200. A whole-word text search at the side is the fallback for names with no definition found. The working tree's changed files are extracted in memory and never cached. Whether a navigator still answers for a diff read again is the core's call, `Navigator::refit`, which every host uses: it compares a fingerprint of what the navigator answers from (the snapshots, each change's ids, and a stat of each working-tree file without one), keeps a tree's or an unmoved index's navigator by taking the changes in place, and otherwise says to make another. `navigate::names` gives the names on a line of code for a frontend to offer, past each language's keywords, literals, comments and an `#include`'s file. `Navigator::references` searches a side for a name's uses, a step of files at a time so a host streams them and stops between files: closest files first, past comments and strings (those over several lines too, followed per language by `navigate::names::Comments`) and the line inside each definition that names it, the lines of the first 200 files with one listed, at most 1000 a file, and the rest counted. A search steps only with the navigator that made it. |
| `suggest` | Closest-name suggestions for "did you mean" hints. |

### `dffy`

The `dffy` binary: CLI, printed output, pager mode, the viewer, the MCP server and the watch broker. It's also a library so the benchmarks can reach it.

| Module | What it owns |
|---|---|
| `cli` | clap definitions. Inserts the default `diff` subcommand. |
| `config` | The config file, every key with its default, value sources. |
| `state` | Remembered viewer settings in `$XDG_STATE_HOME/dffy/state`. |
| `keys` | Named actions and the key tables: the main one, the tree's, a thread's and the command panel's. |
| `docs` | The generated reference pages (`dffy config --markdown`). |
| `theme` | Roles, themes and tints. Tints mix a hue into the background in Oklab. The added, removed, moved and modified hues share one OKLCH lightness and chroma, so their tints read equally strong on any background. A built-in theme's modified color is its palette's blue, unless that blue is a purple or a teal that moved or added code would share (Dracula, Gruvbox, Nord). Those get a blue of the same lightness and chroma. |
| `output` | Terminal facts, color decisions, stdout or a pager process. |
| `print` | Printed output from the repository, streamed file by file: the threads and output budget around the core's per-file steps, and the layout of each file. |
| `encode` | The core model as JSON, the one encoding `--json`, MCP and the control API share. It writes the header, file and end objects of [json.md](json.md) with their hunks, lines and annotations, by hand from the core's per-file steps, and where each thread is placed. A file object can hold a window of its hunks, for hosts that page. |
| `json` | `--json`: `encode`'s objects as JSON Lines, streamed through `print`'s writer. |
| `mcp` | `dffy mcp`, the Model Context Protocol server ([mcp.md](mcp.md)). Hand-written JSON-RPC over stdio on the `plugin` module's JSON, both protocol eras, the tools (with `encode`'s file objects and placements), cursors and the text rendering. The viewer tools go through `control::client`. `definitions` keeps a navigator per version asked about across calls, its index built on threads of its own between them. |
| `control` | The control API ([control.md](control.md)): its newline-delimited JSON on the `plugin` module's JSON, the handshake, capabilities and schema. `control::server` is dffyd's side: registered views, requests handed to their viewers and answers back, subscriptions. `control::viewer` is a viewer's registration. It holds the connection on its own threads, turns requests into commands for the viewer's loop and encodes what the view answers. `control::client` connects from outside and finds the view a command means, for `dffy ctl` and the MCP server. |
| `no_index` | Deciding when two arguments are paths to compare rather than revisions, and naming the comparison (`git difftool`'s temporary copies included). |
| `pager` | Pager mode: the stdin reader and line splitting, streaming the core's `patch` events into rendered rows, and `--color-only`. |
| `plugin` | Renderer plugins: the `render` setting, finding `dffy-render-<name>` on `PATH`, running one with a timeout, the protocol's JSON. Its submodules are below the table. |
| `review` | Review comments: the comments file's format and the operations that write it under a lock. Its submodules are below the table. Nothing in it draws or reads keys. A word that points to an action carries a `Hint`, and the frontend says it with the key it binds. |
| `fuzzy` | Fzf-style matching for the filters of the picker, the panels and the lists. |
| `render` | The core's `view` rows turned into cells (split, unified, stat): widths, wrapping, the role for each line's state, and escaping. Printed output and the viewer share it. It decides nothing about what a line is. |
| `term` | The terminal layer: cell grid, raw mode, input parsing, OSC 52, OSC 7501. |
| `terminal` | Which terminal dffy runs in, innermost first, read from the environment alone. It builds the command that opens a new tab there at a directory, running the shell or a program such as a coding agent a thread is handed to. The directory and each word of the program are always separate arguments. Where there's no such command, it builds the shell-quoted `cd` to copy instead. The viewer runs the command. |
| `tui` | The viewer (submodules below). |
| `daemon` | `dffyd`: the broker, its client and the socket protocol. The process also serves `control`'s socket. |
| `navigator` | `dffy __navigate`, the helper process that holds a repository's navigators and their index for one host, its protocol's values (`wire`, written and read by both sides) and a host's handle on it (`client`). The index lives there so that its exit gives the memory back ([navigate.md](navigate.md)). |
| `doctor` | `dffy doctor`: facts gathered side by side and judged into checks. `--json` and `--verbose` list the checks one by one, and the text report folds them into a row per thing. |
| `agents` | `dffy agents`: the skill it installs where coding agents read skills, and the guide `dffy agents show` prints, naming only what this build has ([agents.md](agents.md)). |
| `bench` | Entry points for the benchmarks. |

The `plugin` submodules:

- `plugin::rpc` is dffy's side of a long-lived plugin process speaking JSON-RPC over stdio, shared by every such kind of plugin. The process starts in its own session, through `dffy __review-plugin`, because std can start one only from unsafe code. It's restarted when it crashes. The module owns `initialize` and the cache of its answer, request ids, pending requests, cancelling and closing. One thread writes requests, building each as it's written. Another reads answers and notifications and hands them to the plugin kind.
- `plugin::review` runs review plugins (`dffy-review-<name>`), one process per repository. It makes the requests, reading the review's comments file into them, applies the comment operations the plugin sends, and hands the rest to the viewer as events.
- `plugin::agent` runs the agent plugin (`dffy-agent`), one process per viewer. It starts on the first ask, its `initialize` is never cached, and closing waits a moment for it to stop its agents. It picks the agent for an ask from what `initialize` listed.

The `review` submodules:

- `review::model` makes the threads from the file, with the local review's drafts merged in and other branches' kept apart. It hands line comments to the core as annotations and builds each file's threads from where the core placed them. When only their text changed, it rebuilds them at once.
- `review::listing` is the comments panel's list: groups, order, filters, search, selection.
- `review::prompt` turns a thread or a selection into the plain-text prompt a coding agent answers: its comments, where it is, the lines around it and the file's changes near it. It trims to a size and marks every cut. Copy as prompt, `dffy comments prompt` and MCP's `thread_prompt` use it.
- `review::session` is the review session with a review plugin, as a state machine. It takes intents, the plugin's answers and pushes, and returns effects for the frontend to carry out: ask the plugin, show a range, place threads again, say something.
- `review::ask` keeps the asks to a coding agent, one per thread, through the same effects. It writes an answer as the agent's reply through the comments file's write.

The viewer's submodules, under `tui`:

| Submodule | What it owns |
|---|---|
| `app` | State and event handling. Input reaches the overlays, the panels and the key tables in a fixed order, and each action is dispatched from here. `app::threads` is the comment threads (the comments file, placing threads, writing comments, the thread's keys, the comments panel). `app::search` is the diff's search, `app::mouse` clicks, drags and the right-click menu, `app::command_panel` the leader key's panel, and `app::draw` a frame's chrome and status bar. `app::staging` hands the lines of uncommitted changes to the loop to stage, unstage or discard, and asks before a discard. `app::checkpoint` has the loop take a checkpoint on its own thread, including before Fix with agent starts the agent. It switches the list to the changes since the latest checkpoint and back, as a new range. `app::review` is review mode's keys and overlays, the review picker and the post dialog, over `review::session`, whose effects it carries out. Updating a review or opening another goes through the change-set thread as a new range, the way watch mode's refreshes do. `app::control` carries out the control API's commands, which arrive as `Event::Control`. It answers requests waiting for a file's diff once the diff is in, and pushes subscribed events. `app::peek` is go to definition: a navigator per version of the diff, made on the first ask about it from everything that differs between the diff's snapshots, whatever paths the view lists, the definitions read shared between them; after a refresh, the peek's version brought up to date at once (a moved version made again, the old navigator kept until the new one's index is built; one that stayed taking the changes again) and the other's on its next ask; the index build, one pool job at a time behind every other; the lookup, cancelled by the next; the peek's keys; and dropping the navigators ten minutes after the last peek, on a deadline the loop sleeps until. |
| `comments` | Drawing comment threads' boxes, and the comments panel over `review::listing`'s list. |
| `view` | The screen layer over the core's `Document`: row heights when wrapping, scrolling and keeping the screen full, opening a file a few rows above its first change, drawing, the overview column, selections and the mouse. The shown file's threads, the text box and the go-to-definition peek are core blocks. `view::Boxes` measures each box by width (folded or not, with the text box in it) for the core and draws it a row at a time, so a frame with boxes on screen is copied until a box's displayed age changes. It reports what `Document::missing` names for the rows drawn in the last frame. `view::select` is the selections: lines, text in a comment, what's under the pointer, and copies. `view::rendered` turns a renderer's blocks into a rendered `File`. |
| `worker` | The change-set thread, and the pool that runs diffs, annotation placement, renderings, the core's `File::prepare` steps and go to definition's requests to the navigator helper off the UI thread. `app` turns the screen's `missing()` into the pool's jobs. The definitions index is built in the helper's process, not on the pool, and the helper pushes how far it is, an event the viewer takes like any other. |
| `tree`, `picker`, `commands`, `help` | The file tree, the file picker, the command panel, the key list. |
| `peek` | Go to definition on screen: the list of names on a line, and the peek, a block of fixed height under the line with a definition's code beside the name's definitions. |
| `chrome` | The frame. |
| `piped` | Diffs from a pipe: the reader thread, its backpressure, and the spill file that keeps patches out of memory until drawn. |
| `editor` | Opening `$VISUAL` or `$EDITOR` at a line. |

### `plugins/markdown`

`dffy-render-markdown`, the Markdown renderer plugin. It turns pulldown-cmark events into blocks of styled lines and writes the protocol's JSON with serde_json. [plugins.md](plugins.md) has the protocol. ```` ```mermaid ```` fences go through `plugins/mermaid`'s library, and one it can't draw stays a code block.

### `plugins/mermaid`

`dffy-render-mermaid`, the Mermaid renderer plugin. It's a library, which the Markdown plugin uses, and a binary that answers the protocol. The binary falls back to the source lines when the library returns nothing.

| Module | What it owns |
|---|---|
| `canvas` | A grid of cells. Lines are kept as the directions each cell joins, so meeting and crossing lines become the right junction in any drawing order. |
| `flowchart` | The flowchart parser. |
| `flowchart::layout` | The layered layout, in the manner of Sugiyama: back edges reversed, longest-path layers, placeholders for long edges, barycenter ordering with subgraphs kept together, least-squares placement, one track per bending edge between layers, and frames. It works in layer/along-layer coordinates, so top-down and left-right charts share it, and bottom-up and right-left are flips. |
| `sequence` | Sequence diagrams: parser, column spacing, and a block per statement. |

Each module draws once, at the narrowest label wrap that fits the width, and measures sizes before drawing. Past the size limits in [plugins.md](plugins.md#the-mermaid-plugin), the library returns nothing. Layout cost grows with edges times layers, and drawing cost with the drawing's area.

### `plugins/review-github`

`dffy-review-github`, the GitHub review plugin ([plugins.md](plugins.md#review-plugins)). It reaches GitHub only through `gh api` and fetches code only with git. It never touches the comments file. dffy sends the comments with each request and applies the operations the plugin sends back, so the file, its lock and its format stay dffy's. `server` holds the JSON-RPC loop, the request threads and the watcher. `github` holds the GraphQL requests and turns review threads into comment operations. `git` holds the fetches and the anchors read from the PR's commits.

### `plugins/agent`

`dffy-agent`, the agent plugin ([plugins.md](plugins.md#agent-plugins)). It runs Claude Code, Codex, Pi or the user's own command on a thread's prompt and returns the answer and its suggestion. It writes nothing. `server` holds the JSON-RPC loop, the request threads and each ask's process group. `agent` holds the probes, the read-only recipes, the reading of each agent's output and the suggestion's extraction.

### `xtask`

`cargo xtask`, not published. It runs the performance gate (`perf`, with its steps `bench-baseline`, `bench-ab`, `bench-noise` and `bench-layouts`), the comparison with other diff tools (`compare`), and `report`, which prints a run's tables from its `results.json`. [benchmarks.md](benchmarks.md) describes them.

## Data flow

The viewer never waits for the whole diff.

1. A change-set thread lists changed files in git's path order and sends them as they're found. For a range that's a tree merge-join. For the working tree it's a parallel stat scan of the index, then the untracked walk. The first files paint before the scan finishes.
2. Once the list is complete, the rename pass pairs deletions with additions and updates the list in place.
3. A worker pool diffs a file when it's about to be drawn. It then computes emphasis and highlighting for the hunks on screen only, as `Document::missing` names them, through the same `File::prepare` steps printed output uses. Once a file's highlighting is ready, a separate job finds the definitions for the scope line in the syntax trees it kept. The syntax diff, when on, runs on the files on screen after the line diff has painted.

   When idle, the pool compiles the highlight queries of the languages in the list, then counts every file's lines for the tree. It counts the files on the tree's screen first, then a screen above and below, and starts again from wherever the tree scrolls to. Counting uses a thread per core, the UI's included. Threads beyond the pool's own count only while no job waits or runs, and every counting thread stops between files when a job waits. Counts of contents seen before come from the disk cache.

   A file with comments has them placed on its diff in the same job, so its threads show with its first frame. When the comments on a loaded file change, its threads are rebuilt at once from where they were placed. A placement job runs only when its line comments changed. A placement made before a later change, or on a diff since replaced, is dropped.
4. Rendering touches only the rows drawn. A view's position is anchored to a row of a hunk, not a screen row, so loading a diff, resizing or switching layout never measures a whole file. Resizing, switching layout and toggling the syntax diff keep the top line where it is.
5. Renderer plugins run last, and only when asked. With the rendered view on, a worker sends the shown file to its renderer once its line diff is loaded. The answer's blocks are diffed as units (`diff::unit_hunks`) into an ordinary `document::File`, built by `view::rendered`, that takes the line diff's place. Without a `render` setting, no renderer code runs.

### Printed output

- Revisions are resolved before anything is written, so errors never land inside a pager.
- Files then stream one by one, each prepared whole by the core's `File::prepare`.
- With renames on, files from the first addition or deletion onward are held until the rename pass has paired them.

### MCP server

- The repository handle, the grammars and the disk cache stay open across calls. Each call resolves its revisions and lists the whole change set again, with renames paired, then diffs only the page it returns, a file per core.

### Pager mode

- stdin is parsed in 64KB chunks, with a flush after each.
- The core's `patch` parser decides what each line is. dffy picks its role and draws it.
- Memory doesn't grow with input size.

## The engine

### Line and intra-line diff

- Line diff: dffy's own histogram diff, falling back to Myers as git does. Lines keep their terminators, so a missing final newline is a difference. imara-diff, a dev-dependency, is the benchmark bar.
- Intra-line diff: the removed and added lines of a block are paired in order, and each pair's tokens are diffed. Pairs that share too little stay unpaired, so unrelated rewrites get no emphasis. Lines over 4KB get no emphasis.

### Renames

dffy's own pass, following git 2.43's diffcore-rename:

1. exact matches;
2. unique file names at 75% similarity or more;
3. scored pairs down to 50%.

### Syntax diff (`--syntax`, `d`)

The output is the line diff's hunks, so applying them produces the new file. A syntax diff never shows a real change as unchanged. Within each block it changes three things:

- Emphasis goes only to tokens with no equal on the other side, or out of order.
- Lines whose code wasn't edited are marked reformatted (`~`) or moved (`»`).
- Matched lines are lined up.

Top-level nodes are anchored by hash first, then matching recurses only inside the changed regions. Parse-error subtrees are matched as opaque pieces, so a parse error doesn't drop the whole file.

| Limit | Value | Over it |
|---|---|---|
| Work per region | 10⁶ work units and 50ms | That region keeps the token diff; the rest of the file keeps its syntax result |
| Time per file | 250ms | Every region not yet matched keeps the token diff |
| File size | 2MB | The file keeps the line diff |
| Parse time | 400ms | The file keeps the line diff |

### Highlighting and grammars

- Highlighting: each file side is parsed once, and the highlight query runs only over the lines asked for.
  - Queries compile once per language, from under 0.1ms to 140ms (Swift), about 430ms for all. They compile in the background once a file of the language appears in the change list. The viewer compiles them one at a time when the pool is idle. Print mode compiles one when its file is queued, while a core isn't parsing. A parse that comes first compiles the query beside it, as does a parse of a file known only by its `#!` line.
  - When both sides are 32KB or more, they parse on two threads at once. Print mode does this only while cores are free. Otherwise the sides parse one after the other, and the new side reuses the old side's tree, so only the lines the diff changed are parsed again. Reuse is skipped when either tree has a parse error, because tree-sitter's error recovery can then differ from a parse from scratch.
  - Sides over 512KB, or whose parse takes over 200ms, stay plain.
- Grammars: Bash, C, C++, C#, CSS, Dockerfile, Elixir, Go, HCL (Terraform), HTML, Java, JavaScript, JSON, Kotlin, Lua, Make, Markdown, PHP, Python, Ruby, Rust, Scala, SQL, Swift, TOML, TSX, TypeScript, XML, YAML and Zig, bundled with tree-sitter. They're about 52MB of the 57MB binary. SQL's alone is about 13MB.
- Queries: each grammar's own highlight query, with two exceptions. dffy writes HCL's itself (`crates/dffy-core/src/highlight/queries/hcl/`) because the grammar crate ships none, and appends a few patterns to Kotlin's. Queries written for Neovim compile unchanged: `#lua-match?` is read as `#match?`, and `@spell` is ignored.
- Scopes: the viewer's scope line comes from a scope query dffy writes per language (`crates/dffy-core/src/highlight/queries/<language>/scopes.scm`).
  - Once a file's highlighting is ready, a separate job walks each side's kept tree, entering only nodes that span several lines (2 to 7% of the parse's time on Rust), and keeps the definitions sorted by line. Each frame looks up the top line by binary search.
  - Highlighting doesn't wait for the walk. Until the walk is done, the title shows the hunk's heading, as it does before highlighting arrives.
  - A side whose spans come from the disk cache has its definitions stored beside them, computed when the spans were stored.
  - Scope queries compile in the background with the highlight query, in 0.3ms (Go) to 20ms (C++).
- Definitions, for go-to-definition: a tags query dffy writes per language (`crates/dffy-core/src/highlight/queries/<language>/tags.scm`, for the 18 languages [viewer.md](viewer.md#go-to-definition) lists), with its members' containers and its re-exports, plus a table of definition macros per language in `highlight/tags.rs` (for C, `SYSCALL_DEFINE0` to `SYSCALL_DEFINE6`, `TRACE_EVENT`, `DEFINE_EVENT`, `DEFINE_FREE` and `DEFINE_*`), read from the macro's arguments because the grammar parses those uses in many shapes. A macro's definition runs to its closing parenthesis, or over the body that follows it. Attribute macros with arguments that the grammar can't parse before a function's type (for C, `__alloc_size`, `__realloc_size`, `__printf` and `__scanf`) are read as spaces for this parse only, so the function around them is found.
  - Each blob is parsed once for them and its definitions stored by blob id, so two revisions share their unchanged files. The walk skips function bodies, and a macro inside one that a parse error left as loose braces.
  - Sides over 512KB, judged by the object's size before it's read, have none and are stored as such. Those whose parse takes over 2s have none and are tried again. Either is reported as the reason. A tags query that doesn't compile is an error the host shows, not an empty result.

### git access

- gix provides the object database, packs, refs and revision parsing.
- dffy implements tree diffs, the status scan and the untracked walk itself, so they stream in path order and can stop early.

## Watcher and broker

### Watcher

- The backend thread never blocks, since a stalled inotify reader overflows the kernel queue.
- The coalescing thread batches paths (8ms quiet, at most 100ms). It drops paths no diff depends on and paths git ignores, and polls when the backend can't watch.
- A batch of more than 4,096 paths goes out as a rescan, and the viewer lists the whole diff again instead of checking each path.
- On Linux, ignored directories that hold no tracked files aren't watched at all.

### `dffyd`

`dffyd` shares one watcher per repository between every view, so inotify use grows with the number of repositories watched, not the number of views. Each view still runs its own engine.

- Versions: the socket name carries the protocol version, so brokers of different versions never talk to each other. Any change to the frame format bumps it.
- Lifetime: dffyd exits 60 seconds after its last subscriber and registered view leave. While anyone is connected it waits on a condition variable, not a timer. Each socket has a lock, so one dffyd serves it per version. A dffyd started while another holds one lock serves only the other socket.
- Slow subscribers: each gets its own write queue and is dropped after a one-second stall, so it can't hold up the others or the watcher.
- Fallback: a client that can't reach the broker watches by itself. Printed output and the pager never use the broker.

### The control API

- One socket per user, `control-v<N>.sock`, mode `0600` in the private runtime directory. On Linux each connection's user is checked too.
- A view's registration is its connection, made after the viewer's first frame. Closing it, or the viewer dying, unregisters the view. dffyd hands requests to the view with its own ids and passes answers back with the view's id added.
- The viewer's loop gets each request as `Event::Control`, already parsed, and answers through a writer thread, so it never waits on the socket. A request on a file not diffed yet shows the file and waits for its diff. Comments it's asked to add go through the comments file's write under its lock, like typed ones.
- Subscriptions: dffyd tells the viewer which events anyone subscribes to. With none, the viewer does nothing per event. With some, it pushes the current state at once, then compares the selection and the top row after each event and pushes 50ms after they stop changing, through the loop's own deadline.

[control.md](control.md) has the protocol. [watch.md](watch.md) has the user-facing side.

## The renderer

dffy has its own terminal layer (`term`) and uses no TUI library.

- Cell grid: the screen is a double-buffered grid of styled cells. Each flush sends only the cells that changed since the last frame.
- Scrolling: when only the diff's position changed since the last frame, the rows still on screen are copied from it, and only the rows that came into view are laid out. The terminal moves the copied rows itself with a scroll region (`CSI t;b r`) and deleted or inserted lines (`CSI n M` / `CSI n L`), so only the new rows and other changes are sent. A scroll region spans whole rows, so with the tree beside the diff, the tree's columns move too and must be sent again. The terminal scrolls only while those cells, counted a third more, are fewer than the diff's cells that moved.
- Synchronized output: every flush is wrapped in `CSI ? 2026 h` / `l`, so terminals that support it never show half a frame.
- Input: raw termios and dffy's own parser, for input that every terminal and multiplexer passes through. The one extension is the kitty keyboard protocol's disambiguate flag, on while dffy has the terminal, so the text box can tell `shift+enter` from `enter`. Its keys read as their legacy forms, and every action has a key that works without it.
- The one query: the background color (OSC 11), with a 200ms timeout and a dark fallback.
- Restore: the terminal is restored on exit, on panic (release builds abort on panic, so a hook does it), on SIGINT/SIGTERM/SIGHUP, and while the editor runs.
- Layout: `render` writes role-tagged rows into a `Canvas`. Roles become escape sequences only when a canvas is written to stdout or copied into the grid, so printed output and the viewer share layout code.

## Cache and state

### Disk cache (`$XDG_CACHE_HOME/dffy`)

- Contents:
  - rename results;
  - highlight spans and syntax diffs of files of 64KB or more;
  - renderer plugins' answers, keyed by the renderer's installed program, both sides, the width and the background;
  - line counts (`git::StatStore`), shared by the viewer's tree, `--stat`, JSON summaries and the MCP server's file lists.
- Line counts: a record holds both sides' blob ids and the counts under one whitespace mode. Working-tree files are keyed by the status scan's hash. A file with a side that has no id unless read (a working-tree file the scan didn't hash, a path compared outside git) isn't kept. A process reads the cache only after spending 2ms counting, and stops reading if its first three lookups miss. It writes its counts only once counting took 100ms in total.
- Keys: an entry's name hashes the dffy-core version, its kind and every input (blob ids, the grammar's identity, options), so entries never need invalidating. A grammar's identity digests its name, ABI and metadata versions, table sizes and query text. Line counts are the exception. Their records are spread over 16 entries per whitespace mode. Each entry is read once per process and rewritten with the records counted since, keeping its newest 256KB. A record never changes meaning, so losing a rewrite to another process costs only a recount.
- Writes go through a temporary file renamed into place. Each entry has a length and a checksum.
- Trimming: down to `cache-size` (256MB), by removing the oldest entries until 80% is left, at most every 10 minutes across processes.
- Definitions: in `symbols/` inside the cache (`SymbolCache`, a type of its own so the shared cache can't be passed for it), with its own 128MB (or `cache-size`, if smaller), so they never evict highlight spans. It counts the blocks its entries take on disk. `dffy doctor` reports both together. A blob's definitions are kept under a digest of the blob id and the grammar's identity with the tags query, so editing a highlight query keeps them. They're packed into 256 entries by the digest's first byte, since most blobs' definitions take a few hundred bytes and an entry of their own would take a disk block. A pack is rewritten whole when something was added, merged with what other processes stored in it since it was read, with the blobs read meanwhile moved to the back. Past its share of 90% of the budget it drops its least recently used blobs, and a blob larger than the share isn't kept, so the packs stay within the budget.
- Speed: a hit is 200 to 500 times faster than recomputing a syntax diff.

### State (`$XDG_STATE_HOME/dffy/state`)

- Remembered viewer settings under a `version` line.
- Each writer re-reads the file, changes only its own keys, writes back keys it doesn't know as they were, and replaces the file atomically.
- A file that doesn't parse counts as empty.
- The config directory is never written.

## Performance

Budgets, measured on the kernel clone with a warm page cache. Cold runs (180–600ms for the status scan) are tracked but not budgeted. Benchmarks report timings and never fail `task ci`.

| Budget | Target | Measured |
|---|---|---|
| First paint, range | 50ms | First 50 paths of `v6.0..v6.1` in 0.8ms, all 13,168 in 24ms; first viewer frame in a pty in 2.6ms |
| First paint, working tree | 50ms | First modified file in 27ms, all tracked files in 54ms, with untracked files 109ms |
| Pager, first output | 20ms | 3.6ms on a 53MB diff; 9.6MB peak RSS for the whole diff |
| Print, first output | — | 7.2ms for the range (6.4ms without highlighting), 30ms for the working tree |
| Frame render | 8ms | 140–235µs for a full 200×60 frame, including the 13,000-file tree |
| Watch refresh | 16ms | About 11ms from the write to the repaint, 8ms of it the debounce |
| Renames on the range | — | 254 of 254 pairs match git, in 33–35ms (git: 194–201ms) |
| Line diff, worst file (1.1MB) | 1.5× imara-diff | 1.9ms, 0.89× imara-diff |
| Syntax diff, five large kernel files | 500ms, 256MB each | 23–327ms for each `dffy --syntax` run, at most 109MB of its own memory; `difftastic` fell back to a line diff on all five |

Measured on a VM with 4 vCPUs of an AMD EPYC 9554P, 15.6GiB of RAM, Ubuntu 24.04.5 and Linux 6.12.115.

To reproduce:

1. Point `DFFY_BENCH_REPO` at a kernel clone with tags `v6.0` and `v6.1`.
2. Run `task perf:full` for the divan benchmarks in `crates/dffy-core/benches/engine.rs` and `crates/dffy/benches/frame.rs` and the end-to-end timings, and `task bench:compare` for the whole-command rows.
3. Turn each run into tables with `cargo xtask report <run-dir>`.

### Checking a change

A change that could affect speed or memory is gated A/B against a build of another commit on the same machine with `task perf` (`cargo xtask perf`). [benchmarks.md](benchmarks.md#checking-a-change-for-regressions) has the commands, the groups and the exact rules. In short:

- `task perf:quick` counts instructions and allocations only (Linux) and runs on every change. `task perf` adds timing and peak memory, for hot paths or when the quick tier fails. `task perf:full` runs every group with fresh layout bands before a release.
- The baseline is the commit given with `--rev`, `HEAD~1` by default, built fresh from a git worktree. The candidate is the checkout, uncommitted changes included. Both builds run in every round, alternating which goes first.
- A timing or peak-memory metric fails when the candidate is consistently worse by more than 1% or the metric's layout band. An instruction count fails above 0.5%. An allocation count or byte total fails on any increase.
- Every run writes `results.json` and a summary to `target/perf/runs/<time>/`, and removes its builds afterwards.

Published numbers come from the Linux VM above. Layout bands hold only for the machine that measured them.

## Testing

| Command | Runs |
|---|---|
| `task ci` | `fmt:check`, `lint` (clippy, warnings are errors), `test`, `docs:check`, `doc`, `deny`, `machete`, `typos`, in order |
| `task test` | Every unit and integration test with cargo-nextest, then doctests |
| `task test:kernel` | The kernel tests in release mode; needs `DFFY_BENCH_REPO` |
| `task perf` | The checkout against a build of the revision given with `--rev` (default `HEAD~1`), every group unless `--only` narrows it ([benchmarks.md](benchmarks.md#checking-a-change-for-regressions)) |
| `task perf:quick` | Every group's instruction and allocation counts only; Linux only |
| `task perf:full` | Every group, measuring layout bands again when missing or more than 30 days old |
| `task bench:compare` | dffy, built from the checkout, against other diff tools: the tables in [benchmarks.md](benchmarks.md) |
| `task docs` | Regenerates [keys.md](keys.md) and the settings table in [config.md](config.md) |
| `task setup` | Installs the tools the other tasks use |

### Kernel tests

- Run them with `DFFY_BENCH_REPO=~/.cache/dffy-bench/linux task test:kernel`, against a kernel clone with tags `v6.0` and `v6.1`. Without the variable they're skipped.
- They check the change list against git's, apply dffy's hunks to reproduce the new files, and run the syntax diff and the watch storm tests.
- They run in release mode because the syntax diff's parse budget is wall-clock time.

### Other tests

| Kind | Where and how |
|---|---|
| Snapshot | insta, inline (`@r"..."`) or as files in `snapshots/` directories. A changed snapshot fails the test. Review it with `cargo insta review` (from cargo-insta, which `task setup` doesn't install). |
| Property | proptest: hunks applied to the old text reproduce the new one, and every reformatted or moved mark the syntax diff makes is checked against the text. Failing seeds are saved in `crates/dffy-core/proptest-regressions/` and rerun on every test run. Keep them. |
| pty | `crates/dffy/tests/tui.rs` runs the built binary in a pseudo-terminal (portable-pty), types keys, resizes, and reads the screen back through a terminal emulator (vt100). |
| CLI | `crates/dffy/tests/cli.rs` runs the binary on fixture repositories with an empty config, state and cache, so the user's environment can't leak in. |

### Generated docs

`task docs:check` regenerates the docs pages into a temporary directory and fails on any difference. After changing a config key, an action, a default key or a remembered setting, run `task docs`.

## Code rules

- Lints live in `[workspace.lints]`. `unsafe_code` is denied, allowed only in the FSEvents backend for FFI. Clippy `all` is denied and `pedantic` warns, and CI fails on any warning.
- MSRV is Rust 1.90, set by tree-sitter.
- The release profile uses fat LTO, one codegen unit and `panic = "abort"`.

## Dependencies

A dependency must be an established crate, added only when a few lines can't do the job. Add it with `cargo add`.

| Crate | Direct dependencies |
|---|---|
| dffy-core | gix, tree-sitter with 30 grammars, ignore, memchr, rustc-hash, thiserror, objc2 (macOS), rustix (Linux) |
| dffy | dffy-core, clap (with clap_complete and clap_mangen), pulldown-cmark, rustix, signal-hook, unicode-width, nix (macOS) |
| `plugins/markdown` | dffy-render-mermaid, pulldown-cmark, serde_json, unicode-width |
| `plugins/mermaid` | serde_json, unicode-width |
| `plugins/review-github` | serde_json |
| `plugins/agent` | rustix, serde_json |
| `xtask` | serde_json, portable-pty, vt100, perf-event2 (Linux) |

- gix is built without `max-performance`, which pulls in an MPL-2.0 crate.
- dffy has no JSON crate. All its JSON (`--json`, MCP, the control API, the plugin protocols) is hand-written.
- `task deny` checks advisories, licenses, bans and sources. `task machete` finds unused dependencies.

## Releases

goreleaser builds the release from `.goreleaser.yaml`:

- macOS (x86_64, aarch64) with the native toolchain;
- Linux (x86_64, aarch64) through cargo-zigbuild;
- a Homebrew cask pushed to `amalucelli/homebrew-tap`.

```sh
task bump VERSION=0.3.0    # set the version in every Cargo.toml and Cargo.lock, then commit
task snapshot              # build the artifacts locally, no tag, no publish
task release TAG=v0.3.0    # sign and push the tag, then publish with goreleaser
```

`task bump` only edits files. `task release` refuses a tag that doesn't match dffy's and dffy-core's Cargo versions, and reads a GitHub token from `gh auth token`. goreleaser reads the tag from the remote, so the tag is pushed before it runs.

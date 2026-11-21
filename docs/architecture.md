# Architecture

For contributors and coding agents. [design.md](design.md) is the approach a change follows: where code goes, ownership, protocols, performance and testing.

- **Priorities, in order:** performance, usability, developer experience.
- **Reference workload:** the Linux kernel repository (97,000 files; `v6.0..v6.1` changes 13,168 of them).

## Crates

The workspace has four kinds of crate:

- `dffy-core`, the library that decides what a diff means. It's versioned on its own and not published to crates.io; its API is 0.x and unstable.
- `dffy`, the binary, which shows it in a terminal. It has no supported API.
- `xtask`, the benchmark tooling.
- The experimental renderer plugins under `plugins/`, programs of their own that are never linked into dffy.

### `dffy-core`

The engine: git access, the line and intra-line diff, the syntax diff, highlighting, patch parsing, the document a frontend shows (files, rows, line states, positions, navigation, search), and annotations placed on its files' lines. It has no terminal code and doesn't assume which process hosts it.

**Core decides, the host draws.** Core decides which lines changed, what each line is (modified, moved, reformatted), the rows of each arrangement, where a position is and where navigation goes, and what a search matches. The host supplies the executor, text measurement (widths, wrapping), colors and drawing. Core has no thread pool or event loop: the host runs the work `Document::missing` names on threads of its own. Some calls fork scoped threads for one job and join them before returning (two sides parsed at once, the status scan, rename scoring); the watcher and the cache's trimming run threads of their own.

**Layers.** A module imports only from its own layer and the layers below it:

| Layer | Modules | Built on |
|---|---|---|
| 0 | `diff`, `patch`, `view`, `conflict` | memchr and rustc-hash. No git, no tree-sitter, no I/O or threads. |
| 1 | `highlight`, `structural` | tree-sitter |
| 2 | `git`, `watch`, `cache` | gix and the filesystem |
| 3 | `document`, `annotation` | Everything below |

Layer 1 is the exception: `highlight` and `structural` store results through `cache` and name blobs by `git::ObjectId`, and the grammar registry hashes grammar identities with gix. `suggest` imports nothing; `git` uses it.

| Module | What it owns |
|---|---|
| `diff` | The hunk model, and the line diff: histogram with a Myers fallback. |
| `diff::intraline` | Token-level emphasis inside changed lines, and the pairing of a change block's removed and added lines, for hunks and for a patch's lines alike. |
| `view` | What a frontend draws, short of how it looks: a hunk's lines and their states (modified, moved, reformatted), headers, rows in the split or unified arrangement, syntax runs, paint and attributes, hunk headings. No widths, colors or cells. |
| `patch` | Unified diffs read as text (`git diff`, `git log -p`, `diff -u`): the line-at-a-time parser and its events, what text lines and combined-diff lines are, and the builder that collects events into commits and files. No I/O: the host reads the input and stores what it keeps. |
| `highlight` | tree-sitter highlighting of the requested lines, and a file's two sides prepared for it. The grammar registry (`Registry`) is a value the host builds and passes in, with the 30 bundled grammars as `Registry::builtin`. Matching a file to a language reads only names and extensions; a grammar is loaded on its language's first parse or query compile. Each grammar has an identity that goes into cache keys. |
| `structural` | The syntax-aware diff: anchoring, per-region matching, the result cache. |
| `conflict` | Merge conflicts in a conflicted file: the marker rule, the regions the markers bound (settled by the index's sides where they read two ways), a choice per region, the text choices resolve the file to, and the preview's hunks. |
| `git` | Revision resolution (through gix), streaming change enumeration, content loading. |
| `git::spec` | `git diff`-style arguments resolved to the two sides of a diff. |
| `git::tree` | Tree-to-tree and tree-to-index diffs as ordered merge-joins that skip equal subtrees. |
| `git::status` | The working-tree side: a parallel stat scan of the index in index order, and the untracked walk. |
| `git::rename` | Rename detection after enumeration, following git's diffcore-rename so pairs and scores match `git diff -M`. |
| `git::changes` | A diff's file list kept current for watch mode, with the parsed index kept loaded. |
| `git::checkpoint` | Checkpoints: the working tree as the uncommitted diff lists it, written as a tree from `HEAD`'s with the status scan's changes and the untracked files, in a commit under `refs/worktree/dffy/checkpoints/<n>`, the last 5 kept per worktree. Never the index, `HEAD` or the working tree. Comparing with one is `DiffSpec::Checkpoint`, the status scan against its tree, untracked files then paired with the tree's by path. |
| `git::conflict` | A conflicted file's conflicts as the index (stages 1 to 3) and the working tree have them, why one can't be resolved by region, and writing a resolution. |
| `git::paths` | Two files or folders compared outside git, behind the same `Repo` handle. Walks both trees in step; a worker per core stats and compares files, and results come back in path order. |
| `watch` | The file watcher: inotify and FSEvents backends, coalescing, ignore rules, polling. |
| `cache` | The content-addressed disk cache shared by every dffy process. |
| `document` | A diff as a frontend shows it, one file at a time. `Document` holds every entry (files, and a piped log's commits) with its diff once prepared, revealed lines, counts, version, saved position and rendered view; the position, and navigation by row, change and file; the syntax-diff, full-text and rendered toggles; the choices made on a conflicted file's regions, which its diff then previews; eviction of least recently shown diffs; the search's matches; and `Document::missing`, the work the rows about to be shown still lack. Moves that need the host's screen come back as a `Settle` the host finishes. `File` is one entry's diff: its parts (hunks and gaps of unchanged lines), rows per arrangement, gap reveal, the overview's marks, search, and rendered views' mapping to their source. `File::prepare` is the per-file work, split per file, hunk and gap so a host runs only what it draws. It reads no files other than the cache's and starts no threads of its own: the host fetches content and schedules the work. |
| `document::blocks` | Blocks: rows a host inserts among a file's, each an id, where it hangs (under the first row, under the row showing a line of one side, or under the last row) and plain text. They're rows of the row model: a part's rows are its own (code, separators, headers) with its blocks placed between them, blocks hung at one place parted by the blank rows `Arrange::parting` asks for, so `Pos`, moving, `Document::missing`, revealing and the full-text toggle count them, and positions on them stay put when rows change around them. A block on a hidden line hangs under its gap's separator. Heights come from the host through `Arrange::measure`, asked only for the parts a call touches and kept per block and width until the host calls `Document::remeasure`; `Document::relayout` keeps the position on the line or block row it showed when the host lays rows out anew. Search reads their text as a second list of matches placed among the lines' (`Matches::get` goes through both in order), and `Document::step_block` walks them across files. What a block shows is the host's. |
| `annotation` | Ranges of a file's lines that something outside the diff holds on to (the viewer's comments are one source), each recorded with its side, its lines and the text around them, and where they land on a version of the file: at their recorded lines when the text still matches, else where their lines and context appear once within 1,000 lines, else lost. `AnnotationSet` stamps each path with the generation its annotations last changed at, so a placement made before a change is told apart and a change to one path re-places that path alone. Placement reads only the diff's two sides, so the host runs it off its main thread; `Document::reveal_annotations` then opens the gap lines around each placed one. What an annotation says is the host's. |
| `suggest` | Closest-name suggestions for "did you mean" hints. |

### `dffy`

The `dffy` binary: CLI, printed output, pager mode, the viewer, the MCP server and the watch broker. It's also a library so the benchmarks can reach it.

| Module | What it owns |
|---|---|
| `cli` | clap definitions; inserts the default `diff` subcommand. |
| `config` | The config file, every key with its default, value sources. |
| `state` | Remembered viewer settings in `$XDG_STATE_HOME/dffy/state`. |
| `keys` | Named actions and the key tables: the main one, the tree's, a thread's and the command panel's. |
| `docs` | The generated reference pages (`dffy config --markdown`). |
| `theme` | Roles, themes and tints. |
| `output` | Terminal facts, color decisions, stdout or a pager process. |
| `print` | Printed output from the repository, streamed file by file: the threads and output budget around the core's per-file steps, and laying each file out. |
| `encode` | The core model as JSON, the one encoding `--json`, MCP and the control API share: the header, file and end objects of [json.md](json.md) with their hunks, lines and annotations, written by hand from the core's per-file steps, and where a thread is placed. A file object can also hold a window of its hunks, for hosts that page. |
| `json` | `--json`: `encode`'s objects as JSON Lines, streamed through `print`'s writer. |
| `mcp` | `dffy mcp`: the Model Context Protocol server ([mcp.md](mcp.md)). Hand-written JSON-RPC over stdio on the `plugin` module's JSON, both protocol eras, the tools (their file objects and placements `encode`'s), cursors and the text rendering. The viewer tools go through `control::client`. |
| `control` | The control API ([control.md](control.md)): its newline-delimited JSON on the `plugin` module's JSON, the handshake, capabilities and schema. `control::server` is dffyd's side: the registered views, requests handed to their viewers and answers back, subscriptions. `control::viewer` is a viewer's registration, its connection held on threads of its own, turning requests into commands for the viewer's loop, and the encoding of what a view answers. `control::client` connects from outside and finds the view a command means, for `dffy ctl` and the MCP server. |
| `no_index` | When two arguments are paths to compare rather than revisions, and what the comparison is called (`git difftool`'s temporary copies included). |
| `pager` | Pager mode: the stdin reader and line splitting, streaming the core's `patch` events into rendered rows, and `--color-only`. |
| `plugin` | Renderer plugins: the `render` setting, finding `dffy-render-<name>` on `PATH`, running one with a timeout, the protocol's JSON. `plugin::rpc`: dffy's side of a long-lived plugin process speaking JSON-RPC over stdio, for every such kind of plugin. It's started in a session of its own (through `dffy __review-plugin`, since std starts one only from unsafe code) and restarted when it crashes. It owns `initialize` and the cache of its answer, request ids, pending requests, cancelling and closing. A thread writes requests, made as they're written; another reads answers and notifications and hands them to the kind. `plugin::review`: review plugins (`dffy-review-<name>`), one such process per repository. It makes the requests, reading the review's comments file into them, applies the comment operations the plugin sends, and hands the rest to the viewer as events. `plugin::agent`: the agent plugin (`dffy-agent`), one such process per viewer, started on the first ask and never cached, whose close waits a moment for it to stop the agents it runs; it picks the agent an ask goes to from what `initialize` listed. |
| `review` | Review comments: the comments file's format and the operations that write it under a lock. `review::model` makes the threads from the file (the local review's drafts merged in, other branches' kept apart), hands its line comments to the core as annotations, and makes each file's threads from where the core placed them, at once again when only their text changed; `review::listing` is the comments panel's list (groups, order, filters, search, selection); `review::prompt` makes a thread or a selection into the plain-text prompt a coding agent answers (its comments, where it is, the lines around it, the file's changes near it), trimmed to a size with every cut marked, for Copy as prompt, `dffy comments prompt` and MCP's `thread_prompt`; `review::session` is the review session with a review plugin as a state machine: it takes intents, the plugin's answers and pushes, and returns effects the frontend carries out (ask the plugin, show a range, place threads again, say something); `review::ask` keeps the asks to a coding agent, one per thread, through the same effects, and writes an answer as the agent's reply through the comments' write. Nothing in it draws or reads keys: words that point to an action carry a `Hint`, which the frontend says with the key it binds. |
| `fuzzy` | Fzf-style matching for the picker's, the panels' and the lists' filters. |
| `render` | Turning the core's `view` rows into cells (split, unified, stat): widths, wrapping, the role for each line's state, and escaping, shared by printed output and the viewer. It decides nothing about what a line is. |
| `term` | The terminal layer: cell grid, raw mode, input parsing, OSC 52, OSC 7501. |
| `terminal` | Which terminal dffy runs in, innermost first, read from the environment alone, and the command that opens a new tab in it at a directory, running the shell or a program (a coding agent a thread is handed to), the directory and each word of the program always arguments of their own; the shell-quoted `cd` copied where there's none. The viewer runs the command. |
| `tui` | The viewer (submodules below). |
| `daemon` | `dffyd`: the broker, its client, and the socket protocol; the process also serves `control`'s socket. |
| `doctor` | `dffy doctor`: the facts, gathered side by side, judged into checks that `--json` and `--verbose` list one by one and the text report folds into a row per thing. |
| `agents` | `dffy agents`: the skill it installs where coding agents read skills, and the guide `dffy agents show` prints, naming only what this build has ([agents.md](agents.md)). |
| `bench` | Entry points for the benchmarks. |

The viewer's submodules, under `tui`:

| Submodule | What it owns |
|---|---|
| `app` | State and event handling: input reaches the overlays, the panels and the key tables in a fixed order, and each action is dispatched from here. `app::threads` is the comment threads (the comments file, placing threads, writing comments, the thread's keys, the comments panel), `app::search` the diff's search, `app::mouse` clicks, drags and the right-click menu, `app::command_panel` the leader key's panel, and `app::draw` a frame's chrome and status bar. `app::staging` hands the lines of uncommitted changes to the loop to stage, unstage or discard, asking before a discard. `app::checkpoint` has the loop take a checkpoint, on a thread of its own and before a Fix with agent starts the agent, and switches the list to the changes since the latest and back, as a new range. `app::review` is review mode's keys and overlays, the review picker and the post dialog, over `review::session`, whose effects it carries out. A review's update or another review goes through the change-set thread as a new range, the way watch mode's refreshes do. `app::control` carries out the control API's commands, which arrive as `Event::Control`, answers those waiting for a file's diff once it's in, and pushes the events subscribed to. |
| `comments` | Drawing comment threads' boxes, and the comments panel over `review::listing`'s list. |
| `view` | The screen layer over the core's `Document`: rows' heights when wrapping, scrolling and keeping the screen full, opening a file a few rows above its first change, drawing, the overview column, selections and the mouse. The threads of the file shown and the text box are the core's blocks there: `view::Boxes` measures each box by width (folded or not, with the text box in it) for the core and draws it a row at a time, so frames are copied with boxes on screen until an age on one reads differently. It reports what `Document::missing` names for the rows drawn in the last frame. `view::select` is the selections: lines, text in a comment, what's under the pointer, and copies. `view::rendered` turns a renderer's blocks into a rendered `File`. |
| `worker` | The change-set thread, and the pool that runs diffs, annotation placement, renderings and the core's `File::prepare` steps off the UI thread. `app` turns the screen's `missing()` into the pool's jobs. |
| `tree`, `picker`, `commands`, `help` | The file tree, the file picker, the command panel, the key list. |
| `chrome` | The frame. |
| `piped` | Diffs from a pipe: the reader thread, its backpressure, and the spill file that keeps patches out of memory until drawn. |
| `editor` | Opening `$VISUAL` or `$EDITOR` at a line. |

### `plugins/markdown`

`dffy-render-markdown`, the Markdown renderer plugin. It turns pulldown-cmark events into blocks of styled lines and writes the protocol's JSON with serde_json. [plugins.md](plugins.md) has the protocol. ```` ```mermaid ```` fences go through `plugins/mermaid`'s library; one it can't draw stays a code block.

### `plugins/mermaid`

`dffy-render-mermaid`, the Mermaid renderer plugin. It's a library, which the Markdown plugin uses, and a binary that answers the protocol. The binary falls back to the source lines when the library returns nothing.

| Module | What it owns |
|---|---|
| `canvas` | A grid of cells. Lines are kept as the directions each cell joins, so meeting and crossing lines become the right junction whatever order they're drawn in. |
| `flowchart` | The flowchart parser. |
| `flowchart::layout` | The layered layout, in the manner of Sugiyama: back edges turned around, longest-path layers, placeholders for long edges, barycenter ordering with subgraphs kept together, least-squares placement, one track per bending edge between layers, and frames. It works in layer/along-layer coordinates, so top-down and left-right charts share it, and bottom-up and right-left are flips. |
| `sequence` | Sequence diagrams: parser, column spacing, and a block per statement. |

Each module draws once, at the narrowest label wrap that fits the width; sizes are measured before drawing. Past the size limits in [plugins.md](plugins.md#the-mermaid-plugin), the library returns nothing: layout cost grows with edges times layers, and drawing cost with the drawing's area.

### `plugins/review-github`

`dffy-review-github`, the GitHub review plugin ([plugins.md](plugins.md#review-plugins)). It reaches GitHub only through `gh api` and fetches code only with git. It never touches the comments file: dffy sends the comments with each request and applies the operations the plugin sends back, so the file, its lock and its format stay dffy's. `server` holds the JSON-RPC loop, the request threads and the watcher; `github` holds the GraphQL requests and turns review threads into comment operations; `git` holds the fetches and the anchors read from the PR's commits.

### `plugins/agent`

`dffy-agent`, the agent plugin ([plugins.md](plugins.md#agent-plugins)). It runs Claude Code, Codex, Pi or a command of the user's on a thread's prompt and returns the answer and its suggestion; it writes nothing. `server` holds the JSON-RPC loop, the request threads and each ask's process group; `agent` holds the probes, the read-only recipes, the reading of each agent's output and the suggestion's extraction.

### `xtask`

`cargo xtask`, not published: the performance gate (`perf`, with its steps `bench-baseline`, `bench-ab`, `bench-noise` and `bench-layouts`), the comparison with other diff tools (`compare`), and `report`, which prints a run's tables from its `results.json`. [benchmarks.md](benchmarks.md) describes them.

## Data flow

The viewer never waits for the whole diff.

1. A change-set thread lists changed files in git's path order and sends them as they're found. For a range that's a tree merge-join; for the working tree, a parallel stat scan of the index, then the untracked walk. The first files paint before the scan finishes.
2. Once the list is complete, the rename pass pairs deletions with additions and updates the list in place.
3. A worker pool diffs a file when it's about to be drawn, then computes emphasis and highlighting for the hunks on screen only, as the core's `Document::missing` names them for the rows drawn and through the same `File::prepare` steps printed output uses. Once a file's highlighting is ready, a job of its own finds the definitions for the scope line in the trees it kept. The syntax diff, when on, runs on the files on screen after the line diff has painted. When idle, the pool compiles the highlight queries of the languages in the list, then counts every file's lines for the tree: the files on the tree's screen first, then those a screen above and below it, again from wherever the tree scrolls to. Counting takes a thread per core, the UI's included. The threads beyond the pool's own count only while no job waits or runs, and every counting thread stops between files when a job waits. Counts of contents counted before come from the disk cache.

   A file with comments has them placed on its diff in the same job, so its threads show with its first frame. When the comments change on a file already loaded, its threads are made again at once from where they were placed, and a placement job runs only when its line comments changed; a placement made before a later change, or on a diff since replaced, is dropped.
4. Rendering touches only the rows drawn. A view's position is anchored to a row of a hunk instead of a screen row, so loading a diff, resizing or switching layout never measures a whole file; resizing, switching layout or the syntax diff keeps the line at the top there.
5. Renderer plugins run last, and only when asked. With the rendered view on, a worker sends the file shown to its renderer once its line diff is loaded. The answer's blocks are diffed as units (`diff::unit_hunks`) into an ordinary `document::File` (built by `view::rendered`) that takes the line diff's place. Without a `render` setting, nothing about renderers happens.

### Printed output

- Revisions are resolved before anything is written, so errors never land inside a pager.
- Files then stream one by one, each prepared whole by the core's `File::prepare`.
- With renames on, files from the first addition or deletion on are held until the rename pass has paired them.

### MCP server

- The repository handle, the grammars and the disk cache stay open across calls; each call resolves its revisions and lists the whole change set again (renames paired), then diffs only the page it returns, a file per core.

### Pager mode

- stdin is parsed in 64KB chunks, with a flush after each.
- The core's `patch` parser decides what each line is; dffy picks its role and draws it.
- Memory doesn't grow with input size.

## The engine

### Line and intra-line diff

- **Line diff:** dffy's own histogram diff, falling back to Myers as git does. Lines keep their terminators, so a missing final newline is a difference. imara-diff (a dev-dependency) is the benchmark bar.
- **Intra-line diff:** removed and added lines of a block are paired in order, and each pair's tokens are diffed. Pairs that share too little stay unpaired, so unrelated rewrites aren't emphasized. Lines over 4KB get no emphasis.

### Renames

dffy's own pass, following git 2.43's diffcore-rename:

1. exact matches;
2. unique file names at 75% similarity or more;
3. scored pairs down to 50%.

### Syntax diff (`--syntax`, `d`)

- **Output:** the line diff's hunks, so applying them produces the new file. A syntax diff never shows a real change as unchanged.
- **What it changes, within each block:**
  - emphasis goes only to tokens with no equal on the other side, or out of order;
  - lines whose code wasn't edited are marked reformatted (`~`) or moved (`»`);
  - matched lines are lined up.
- **Matching:** top-level nodes are anchored by hash first, then matching recurses only inside the changed regions. Parse-error subtrees are matched as opaque pieces, so a parse error doesn't drop the whole file.
- **Limits:**

| Limit | Value | Over it |
|---|---|---|
| Work per region | 10⁶ work units and 50ms | That region keeps the token diff; the rest of the file keeps its syntax result |
| Time per file | 250ms | Every region not yet matched keeps the token diff |
| File size | 2MB | The file keeps the line diff |
| Parse time | 400ms | The file keeps the line diff |

### Highlighting and grammars

- **Highlighting:** a file side is parsed once, and the highlight query runs only over the lines asked for. Queries compile once per language (under 0.1ms to 140ms, Swift's; about 430ms for all), in the background once a file of the language appears in the change list: in the viewer when the pool is idle, one at a time; in print mode when the file is queued, while a core isn't parsing. A parse that comes first compiles the query beside it, as does one of a file known by its `#!` line alone. When both sides are 32KB or more they parse on two threads at once, in print mode only while cores are free. Otherwise they parse one after the other, and the new side is parsed from the old side's tree, so only the lines the diff changed are parsed again; not when either tree has a parse error, as tree-sitter's error recovery can then come out different from a parse from scratch. Sides over 512KB, or whose parse takes over 200ms, stay plain.
- **Grammars:** Bash, C, C++, C#, CSS, Dockerfile, Elixir, Go, HCL (Terraform), HTML, Java, JavaScript, JSON, Kotlin, Lua, Make, Markdown, PHP, Python, Ruby, Rust, Scala, SQL, Swift, TOML, TSX, TypeScript, XML, YAML and Zig (tree-sitter, bundled). They're about 52MB of the 57MB binary; SQL's alone is about 13MB.
- **Queries:** each grammar's own highlight query, except HCL's, which dffy writes itself (`crates/dffy-core/src/highlight/queries/hcl/`) as the grammar crate ships none, and Kotlin's, which gets a few patterns appended. Queries written for Neovim compile as they are: `#lua-match?` is read as `#match?`, and `@spell` is ignored.
- **Scopes:** the viewer's scope line comes from a scope query dffy writes per language (`crates/dffy-core/src/highlight/queries/<language>/scopes.scm`). Once a file's highlighting is ready, a job of its own walks each side's tree the highlighting kept, entering only nodes that span several lines (2 to 7% of the parse's time on Rust), and keeps the definitions sorted by line; each frame looks the top line up by binary search. The highlighting doesn't wait for the walk, and until it's done the title shows the hunk's heading, as it does before the highlighting arrives. A side whose spans come from the disk cache has its definitions stored beside them, found when the spans are computed to store. Scope queries compile in the background with the highlight query, 0.3ms (Go) to 20ms (C++).

### git access

- **gix:** the object database, packs, refs and revision parsing.
- **dffy's own:** tree diffs, the status scan and the untracked walk, so they stream in path order and can stop early.
## Watcher and broker

### Watcher

- **Backend thread:** never blocks, since a stalled inotify reader overflows the kernel queue.
- **Coalescing thread:** batches paths (8ms quiet, at most 100ms), drops paths no diff depends on and paths git ignores, and polls when the backend can't watch.
- **Ignored directories:** on Linux, ignored directories that hold no tracked files aren't watched at all.

### `dffyd`

`dffyd` shares one watcher per repository between every view, so inotify use depends on how many repositories are watched, whatever the number of views. Each view still runs its own engine.

- **Versions:** the socket name carries the protocol version, so brokers of different versions never talk to each other. Any change to the frame format bumps it.
- **Lifetime:** dffyd exits 60 seconds after its last subscriber and registered view leave, waiting on a condition variable rather than a timer while anyone is there. Each socket has a lock, so one dffyd serves it per version; a dffyd started while another holds one serves the other alone.
- **Slow subscribers:** each gets its own write queue and is dropped after a one-second stall, so it can't hold up the others or the watcher.
- **Fallback:** a client that can't reach the broker watches by itself. Printed output and the pager never use it.

### The control API

- **One socket per user**, `control-v<N>.sock`, mode `0600` in the private runtime directory; on Linux each connection's user is checked too.
- **A view's registration is its connection**, made after the viewer's first frame: closing it, or the viewer dying, unregisters it. dffyd hands requests to the view with ids of its own and passes answers back, adding the view's id.
- **The viewer's loop** gets each request as `Event::Control`, already parsed, and answers through a writer thread, so it never waits on the socket. A request on a file not diffed yet shows the file and waits for its diff. Comments it's asked to add go through the comments file's write under its lock, as typed ones do.
- **Subscriptions:** dffyd tells the viewer which events anyone subscribes to. With none, the viewer does nothing per event; with some, it pushes what's there at once, then compares the selection and the top row after each event and pushes 50ms after they stop changing, through the loop's own deadline.

[control.md](control.md) has the protocol.

[watch.md](watch.md) has the user-facing side.

## The renderer

dffy has its own terminal layer (`term`); it uses no TUI library.

- **Cell grid:** the screen is a double-buffered grid of styled cells. Each flush sends only the cells that changed since the last frame.
- **Scrolling:** when only the diff's position changed since the last frame, the rows still on screen are copied from it and only the rows that came into view are laid out. The terminal moves the copied rows itself (a scroll region, `CSI t;b r`, and deleted or inserted lines, `CSI n M` / `CSI n L`), so only the new rows and what else changed are sent. A scroll region spans whole rows, so with the tree beside the diff its columns move too and are sent again; the terminal scrolls only while those cells, counted a third more, are fewer than the diff's cells that moved.
- **Synchronized output:** every flush is wrapped in `CSI ? 2026 h` / `l`, so terminals that support it never show half a frame.
- **Input:** raw termios and dffy's own parser, for input that every terminal and multiplexer passes through. The one extension is the kitty keyboard protocol's disambiguate flag, on while dffy has the terminal, so the text box tells `shift+enter` from `enter`; its keys read as their legacy forms, and every action has a key without it.
- **The one query:** the background color (OSC 11), with a 200ms timeout and a dark fallback.
- **Restore:** the terminal is put back on exit, on panic (release builds abort on panic, so a hook does it), on SIGINT/SIGTERM/SIGHUP, and while the editor runs.
- **Layout:** `render` writes role-tagged rows into a `Canvas`. Roles become escape sequences only when a canvas is written to stdout or copied into the grid, so printed output and the viewer share layout code.

## Cache and state

### Disk cache (`$XDG_CACHE_HOME/dffy`)

- **Contents:**
  - rename results;
  - highlight spans and syntax diffs of files of 64KB or more;
  - renderer plugins' answers, keyed by the renderer's program as installed, both sides, the width and the background;
  - line counts (`git::StatStore`), shared by the viewer's tree, `--stat`, JSON summaries and the MCP server's file lists. A record holds both sides' blob ids and the counts under one whitespace mode. Counts for working-tree files are keyed by the status scan's hash. Files with a side that has no id without reading it (working-tree files the scan didn't hash, paths compared outside git) aren't kept. A process reads the cache only once it has spent 2ms counting, and stops reading if its first three lookups find nothing; it writes its counts only once they took 100ms to count in all.
- **Keys:** an entry's name hashes the dffy-core version, its kind and every input (blob ids, the grammar's identity, options), so entries never need invalidating. A grammar's identity digests its name, ABI and metadata versions, table sizes and query text. Line counts are the exception: their records are spread over 16 entries per whitespace mode, each read once per process and rewritten with the records counted since, keeping its newest 256KB. A record never changes meaning, so a rewrite lost to another process's costs only a recount.
- **Writes:** go through a temporary file renamed into place. Each entry has a length and a checksum.
- **Trimming:** to `cache-size` (256MB), by removing the oldest entries down to 80%, at most every 10 minutes across processes.
- **Speed:** a hit is 200 to 500 times faster than recomputing a syntax diff.

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

All of them need `DFFY_BENCH_REPO`.

### Checking a change

A change that could affect speed or memory is gated A/B against a build of another commit on the same machine, with `task perf` (`cargo xtask perf`). [benchmarks.md](benchmarks.md#checking-a-change-for-regressions) has the commands and the exact rules.

- **Tiers:** `task perf:quick` counts instructions and allocations only, on every change (Linux). `task perf` adds timing and peak memory, for hot paths or when the quick tier fails. `task perf:full` runs every group before a release.
- **Baseline:** the commit given with `--rev`, `HEAD~1` unless given, built fresh by every run from a git worktree of that commit. The candidate is the checkout, uncommitted changes included. Each run removes its builds afterwards, keeping only `target/perf/runs/` and `target/perf/layout.tsv`.
- **What runs:** the divan benchmarks, the `first_frame`, `count_pass` and `first_paint` examples, dffy printing the kernel range and reading its patch as a pager, and the viewer on the range with 300 review threads, timed through `m` and page down; the quick tier prints and pages a 421-file subset of the range, without the viewer timings. Both builds run in every round, alternating which goes first.
- **Groups:** `perf:quick` and `perf:full` measure every group; `task perf` does too unless `--only` narrows it, e.g. `task perf -- --only frame,print`.
- **Timing and peak memory:** a metric fails when the candidate is worse consistently across rounds and by more than 1% or the metric's layout band, whichever is larger, or when its instruction count fails.
- **Instruction counts:** on Linux, user-space instructions are counted too, and a count more than 0.5% above the baseline's fails.
- **Allocations:** allocation counts and bytes per iteration fail on any increase.
- **Layout bands:** how far each metric moves between builds with different code layouts, kept in `target/perf/layout.tsv` for every baseline and measured again by `perf:full` when missing or more than 30 days old, or for a group alone when the file has no band for it. `perf` and `perf:quick` never calibrate.
- **Results:** every run, and `task bench:compare`, writes `results.json` and a summary to `target/perf/runs/<time>/`; `cargo xtask report <run-dir>` prints its tables as markdown.

Published numbers come from the Linux VM above. Layout bands hold only for the machine that measured them.

## Testing

| Command | Runs |
|---|---|
| `task ci` | `fmt:check`, `lint` (clippy, warnings are errors), `test`, `docs:check`, `doc`, `deny`, `machete`, `typos`, in order |
| `task test` | Every unit and integration test with cargo-nextest, then doctests |
| `task test:kernel` | The kernel tests in release mode; needs `DFFY_BENCH_REPO` |
| `task perf` | The checkout against a build of the revision given with `--rev` (default `HEAD~1`), every group unless `--only` narrows it, failing on a slowdown over 1% that's consistent across rounds, more instructions or an allocation increase (see [benchmarks.md](benchmarks.md#checking-a-change-for-regressions)) |
| `task perf:quick` | Every group's instruction and allocation counts only; Linux only |
| `task perf:full` | Every group, measuring layout bands again when missing or more than 30 days old |
| `task bench:compare` | dffy, built from the checkout, against other diff tools: the tables in [benchmarks.md](benchmarks.md) |
| `task docs` | Regenerates [keys.md](keys.md) and the settings table in [config.md](config.md) |
| `task setup` | Installs the tools the other tasks use |

### Kernel tests

- Run with `DFFY_BENCH_REPO=~/.cache/dffy-bench/linux task test:kernel`, against a kernel clone with tags `v6.0` and `v6.1`. Without the variable they're skipped.
- They check the change list against git's, apply dffy's hunks to reproduce new files, and run the syntax diff and watch storm tests.
- They run in release mode because the syntax diff's parse budget is wall-clock time.

### Other tests

| Kind | Where and how |
|---|---|
| Snapshot | insta, inline (`@r"..."`) or as files in `snapshots/` directories. A changed snapshot fails the test; review it with `cargo insta review` (from cargo-insta, which `task setup` doesn't install). |
| Property | proptest: hunks applied to the old text reproduce the new one, and every reformatted or moved mark the syntax diff makes is checked against the text. Failing seeds are saved in `crates/dffy-core/proptest-regressions/` and rerun on every test run; keep them. |
| pty | `crates/dffy/tests/tui.rs` runs the built binary in a pseudo-terminal (portable-pty), types keys, resizes, and reads the screen back through a terminal emulator (vt100). |
| CLI | `crates/dffy/tests/cli.rs` runs the binary on fixture repositories with an empty config, state and cache, so the user's environment can't leak in. |

### Generated docs

- `task docs:check` regenerates the docs pages into a temporary directory and fails on any difference.
- After changing a config key, an action, a default key or a remembered setting, run `task docs`.

## Code rules

- Lints live in `[workspace.lints]`: `unsafe_code` is denied (allowed only in the FSEvents backend, for FFI), clippy `all` is denied and `pedantic` warns, and CI fails on any warning.
- MSRV is Rust 1.90, set by tree-sitter.
- The release profile uses fat LTO, one codegen unit and `panic = "abort"`.

## Dependencies

A dependency must be an established crate, added only when a few lines can't do the job. Add it with `cargo add`.

| Crate | Direct dependencies |
|---|---|
| dffy-core | gix, tree-sitter with 30 grammars, ignore, memchr, rustc-hash, thiserror, objc2 (macOS), rustix (Linux) |
| dffy | dffy-core, clap (with clap_complete and clap_mangen), rustix, signal-hook, unicode-width, nix (macOS) |
| `plugins/markdown` | dffy-render-mermaid, pulldown-cmark, serde_json, unicode-width |
| `plugins/mermaid` | serde_json, unicode-width |
| `xtask` | serde_json, portable-pty, vt100 |

- **gix** is built without `max-performance`, which pulls in an MPL-2.0 crate.
- **JSON:** dffy has no JSON crate; the renderer protocol's, `--json`'s and the MCP server's JSON is hand-written.
- **Checks:** `task deny` checks advisories, licenses, bans and sources; `task machete` finds unused dependencies.

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

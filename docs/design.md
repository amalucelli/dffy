# Design

The rules a change to dffy follows, for contributors and coding agents. [architecture.md](architecture.md) maps the crates and modules as they are. This page says where a new piece goes and what it must keep true. A plan that breaks a rule names the rule and the reason before any code is written.

## Where things go

- dffy-core owns the model: what changed and what each line is, the rows of each arrangement, positions, navigation, search, blocks among a file's rows, and where annotations land. It has no terminal code and knows no code host. It doesn't assume a host process either. The host runs its work on its own threads and supplies text measurement and drawing.
- Frontends are thin over the model. The viewer, the pager, printed output, `--json`, the MCP server and the control API each read the model and present it. What two frontends share lives in a layer with no frontend's types: the review layer (the comments file, its threads, the review session) and the encoder (the one JSON form of the model).
- **When a feature only works by teaching a frontend a special case, the model is missing a concept.** The signs: the viewer counting rows the core doesn't know, a walker of its own, a cache to hide a mismatch. Add the concept to the model so every frontend gets it, instead of patching the view. A frontend decides how things look and how input maps to actions, never what exists.
- Shared layers say what they mean, not how it looks. A word that points to an action carries a hint, and the frontend says it with the key it binds. Escaping, colors and roles happen where the text is drawn.
- Layers in dffy-core import downward only, in the order of architecture.md's layer table.

## Ownership

- Each piece of state has one owner, and every change goes through that owner's write. The comments file belongs to dffy. A typed comment, `dffy comments apply`, the control API's `comments/add` and a review plugin's operations all go through the same write, under the file's lock.
- Anyone else asks the owner through a small versioned protocol and never touches the state. A review plugin returns data. It never writes the comments file or calls back into dffy. A control API client asks the viewer, which runs the request between frames like a key press.
- The owner of a fact states it. Only the host knows a review's head, the last reviewed head and whether a thread is resolved there. That comes from the plugin, and dffy takes it as given instead of working it out.
- A file with several writers, such as the remembered state, has each writer change only its own keys and write the others back as it read them.

## Integrations

- Plugins are separate processes and data sources. dffy starts them, asks them and draws their answers. They have no screen, never prompt, and run nothing inside dffy. dffy has no scripting.
- Code-host logic lives only in plugins. dffy-core and the viewer hold no host's names, ids, URLs or rules. A review plugin names its command, its nouns, its events and its labels, and dffy draws them. A host's own fields travel under the plugin's prefix. dffy keeps them and hands them back unread.
- More hosts are coming. Before adding a message, field or UI concept, check it against GitHub (pending reviews, threads), GitLab (merge requests, draft notes, discussions, approvals), Gerrit (patch sets, drafts, ported comments, votes on several labels) and a review with no host. A field only one host can fill is either optional, with a meaning any host could give it, or the plugin's own. The "Designed for" list in [plugins.md](plugins.md#the-review-protocol-version-1) is the checklist for the protocol, and [the table below](#designed-for) for the comment fields.
- dffy never touches the network. Plugins do.

### Designed for

How the [comments file](comments.md#the-file-version-1)'s fields and the protocol's post map onto the next hosts. GitHub's mapping is dffy-review-github's ([review-github.md](review-github.md)).

| Field | GitLab merge request | Gerrit change |
|---|---|---|
| Thread | A discussion. Its first note is the first comment. | A comment thread. A comment on the patch set's parent is on the `old` side. |
| General comment | A note on the merge request itself, or a system note. | A change message, votes included, of kind `review` with the vote as its `label`. |
| `ref` | The note, once posted. | The comment, once published. |
| `pending` | A draft note on the host. Before that it's a draft here. | A draft on the server. |
| `outdated` | When GitLab marks the discussion outdated. | A comment written on an earlier patch set whose lines changed since, as the plugin says. It's placed by its anchor, taken from that patch set's file. |
| `resolved`, `host_resolved` | The discussion is resolved as a whole, on its first comment. | The thread's state is its last comment's in Gerrit. The plugin gives it on the first comment. |
| `resolves` | A resolve sent by discussion id. A draft reply that resolves is a reply here plus a resolve from `resolves`. | A reply that marks the thread resolved. |
| Events | A submitting event publishes the draft notes together. An approval is an event. | Drafts are published with a reply. |
| `labels` | None. An approval is an event, not a label. | The change's labels, such as Code-Review and Verified. A `post`'s answer repeats them once it changed a vote. |
| Session head | The merge request's head. | The patch set's commit. A newer patch set is `head/moved`. |
| Plugin's own fields | A note's position: its commits and line codes. | A comment's character range on part of a line. The comment keeps the whole lines. |

A locked GitLab discussion takes no reply, so the plugin's plan skips one and says why. A review with no host, kept in the repository, gives each comment a `ref` of the plugin's own when it's posted, and has no `pending` and no `url`.

## Protocols

- Every protocol is versioned. The version is in the handshake, and in the socket's name where there is a socket. Peers of different versions refuse each other and say which version they speak.
- Both sides ignore unknown fields, so adding an optional field needs no new version.
- A session protocol negotiates capabilities at connect. Each peer states once what it can do, and the other never asks for what wasn't offered.
- An experimental protocol has one version and changes in place. dffy and the implementer in this repository change together. There's no compatibility layer while there's a single implementer.
- The model has one JSON encoding. `--json`, MCP and the control API share the encoder's objects and add only their envelopes, so their schemas can't drift. A change that breaks a consumer bumps the schema's version.
- Each protocol and format has one page in `docs/` that is its contract ([plugins.md](plugins.md), [control.md](control.md), [json.md](json.md), [mcp.md](mcp.md), [comments.md](comments.md)). Tests hold the code to it.

## Performance

Speed comes first. [architecture.md](architecture.md) has the priorities and budgets. The reference workload is the Linux kernel's `v6.0..v6.1`.

- Add nothing to the first-frame path. A feature's work starts after the first frame, as a plugin's start and the control API's registration do, or runs only for the rows on screen. Work that must come first, such as asking a review plugin for its command name when the arguments could be that command, is cached and capped by a timeout.
- Do no per-frame work. Compute what a frame needs when its input changes, and look it up while drawing.
- Don't poll. dffy waits on events (the watcher, a process's output, a socket) or on a deadline its loop sleeps until. When idle, nothing wakes. The watcher polls only where the OS can't watch. A plugin that must ask its host owns its cadence and pauses when told the user is idle.
- Compute once per load, off the main thread. Per-file work (the diff, placement, highlighting, scopes) runs in the host's worker pool once per version of the file. Each result is stamped with its version, and a result for an older version is dropped instead of waited for.
- Measure against `main`, before and after, with the perf gate ([benchmarks.md](benchmarks.md#checking-a-change-for-regressions)). The gate that counts runs on the reference Linux machine described in [benchmarks.md](benchmarks.md#setup). Each feature path gets a benchmark scenario, so later changes are gated on it too.

## User experience

- Show a problem where the user acts on it. A draft that won't post as written says so in its heading as soon as it's written, not after the post fails.
- Every action has one plain key: a letter or another key that every terminal, multiplexer and phone keyboard sends. No chords that need an extended keyboard protocol. A key that such a protocol enables is only an extra. In the text box `enter` sends, and `alt+enter` and `ctrl+j` start a new line everywhere. `shift+enter`, which needs the kitty keyboard protocol, is an addition to them. Actions have names, and the keys in the key tables can be rebound ([keys.md](keys.md) is generated from them).
- The right-click menu is a shortcut that teaches the keys. Each entry is something a key does in that spot, shown with its key, and choosing it does what the key does. Nothing lives only in the menu or only on the mouse.
- Nothing fails silently. Every outcome is stated: a result in the status bar, a failure until it's seen, a fallback with its reason (lines instead of the rendered view, the line diff instead of the syntax diff).

## Testing

- Test the contract through its real consumer: the CLI through the built binary, the viewer in a pseudo-terminal, a protocol through the program on its other side, a wire format through fixtures both sides check.
- Stub programs are files under a `tests/fixtures/stubs/` directory, written into a temporary directory with `include_str!`. Never inline a shell script in a test.
- Write the failing test first for a change in behavior.
- Write one case per behavior. Add an input only when it takes a new path. Use property tests where values round-trip (hunks applied, positions through rows) and snapshots for screens.
- Keep the user's environment out. A test that runs the binary gives it an empty config, state and cache.

## How a change lands

1. Write a plan with a verifiable goal: "this scenario stays within its band" or "this test passes", not "make it work". The plan names the files of each step, the order and how each step is validated.
2. Get an independent review per phase, from a person or an agent that didn't build it.
3. Pass the checks in [AGENTS.md](../AGENTS.md).
4. Measure anything near startup, scrolling or another hot path against `main` ([Performance](#performance)). The numbers go in the pull request.
5. Docs state the contract as it is now. History and measurements go in commits and pull requests.

## Guides

### Add a review plugin

1. Write an executable named `dffy-review-<name>`, in any language, and put it on `PATH`. The name uses lowercase letters, digits, `-` and `_`. In this repository it's a crate under `plugins/<name>` like `plugins/review-github`, and the workspace picks up `plugins/*`.
2. Speak JSON-RPC 2.0 on stdin and stdout, one message per line ([plugins.md](plugins.md#the-review-protocol-version-1)). Answer `initialize` at once, from the plugin alone, with [the description](plugins.md#the-description): `protocol`, `command`, `noun` and `nouns`, and optionally `events` and `capabilities`.
3. Map the host onto the requests: `list`, `detect`, `prepare` and `pull` (a `session` notification as soon as the commits are in the clone, then `comments/apply` and `progress`), `dryRun` (plans only, sends nothing), `post` and `doctor`. Answer a request the plugin doesn't support with `-32601`.
4. Return data, never state. Comments go to dffy as `comments/apply` operations. The host's fields go on comments with the plugin's name as prefix, as `github_thread`. Caches go in the plugin's own data directory.
5. Answer each request on its own thread, so a `dryRun` answers while a `prepare` downloads. Stop a cancelled request where possible and answer it with `-32800`.
6. If the host's changes can be watched, describe `"capabilities":{"watch":true}`. Push `head/moved` and `comments/changed` for the review named in `watch`, at a cadence the plugin owns, and pause on `idle/changed`.
7. If the protocol lacks a concept, change the protocol. Don't work around it in the plugin. Check the concept against the hosts under "Designed for", then change [plugins.md](plugins.md), the parsing in `crates/dffy/src/plugin/review.rs`, its handling in `crates/dffy/src/review/session.rs` and `plugins/review-github` in one change.
8. Test against the contract. `crates/dffy/tests/fixtures/review-protocol/` holds a comments file and, in `requests.json`, the requests dffy makes for it. dffy's `requests_carry_what_the_protocol_says_and_never_the_file` (`crates/dffy/src/plugin/review/tests.rs`) and dffy-review-github's `the_harness_carries_what_dffy_carries` (`plugins/review-github/tests/review.rs`) both check it. Stub the host's tools as files, as `plugins/review-github/tests/fixtures/stubs/gh.sh` does.
9. Write a page for what it does and keeps (`docs/review-<host>.md`, like [review-github.md](review-github.md)) and link it from [docs/README.md](README.md). Add its builds to `.goreleaser.yaml`, and its name to `install.sh`'s `--plugin` list and the Taskfile's `install:plugins`.

### Add an agent recipe

A recipe says how `dffy-agent` runs a coding agent's CLI so that an ask can only read, and how it reads what the CLI prints. Each recipe is an entry of `RECIPES` in `plugins/agent/src/agent.rs`.

1. Check the CLI's latest release first: a headless mode that takes the prompt on stdin, the flags that restrict it to reading (tools, sandbox, MCP servers, the repository's own config, network), its output, how a run fails and how it reports being logged out. An agent with no read-only mode an ask can rely on gets no recipe. It can still be a user's command, as Amp is.
2. Add a `Recipe`: its `name` (the program on `PATH`, and its name in `initialize`), `args`, the `worktree_flag` when the CLI wants the worktree named, the `mark` its replies carry, what the recipe allows for `doctor` (`said`, plus a few-word `brief` for its row), its `login` (`Login::Status` for a status command whose exit 1 means logged out, `Login::Stderr` for the words a failed start prints) and its `log_in` command.
3. Write the event reader, a function beside `claude_event`, `codex_event` and `pi_event` that reads one JSON line. It sets the stream's `answer`, `failure` and `error` by the rule its CLI needs: Claude's final result, Codex's failed turn that outweighs any answer, Pi's last message that outweighs earlier ones. It returns the progress to show, using the words in the protocol's `progress` row or a new word documented there.
4. Target only the latest release. There are no version checks or floors. The agent is available when it's on `PATH`, and `--version` runs only for `doctor`. A release that breaks the recipe gets the recipe fixed. The prompt goes on stdin, the agent runs in the worktree, and there's no login flow.
5. Add a stub, `plugins/agent/tests/fixtures/stubs/<name>.sh`, modeled on `pi.sh`. It answers `--version` and lists the recipe's flags on `--help`, except the one a test puts in `unknown`. It writes its arguments, directory and stdin under `$STUB/<name>`, and prints the `answer.jsonl`, `stderr` and `status` a test puts there. Add its directory to `Fixture::new` and a helper that installs it, in `plugins/agent/tests/agent.rs`.
6. Add fixtures, `plugins/agent/tests/fixtures/<name>-*.jsonl`, shaped on a real run's output: an answer with the progress it goes through, a suggestion, and each way a run fails.
7. Test through the built plugin: its arguments, stdin and directory, the answer, the suggestion, each failure, not logged in, and being listed when on `PATH` with its `doctor` rows. Its `not installed` row joins the existing `doctor` tests and `doctor`'s result in `crates/dffy/tests/fixtures/agent-protocol/messages.json`, whose check count `crates/dffy/src/plugin/agent/tests.rs` asserts.
8. Document the recipe, how its answer and failures are read, and what stays possible, in [plugins.md](plugins.md#the-agents-and-their-recipes), with its name, progress words and log-in command in the protocol's tables. Update the other pages that name the agents: the `agent` setting's text in `crates/dffy/src/config.rs` (then run `task docs`), [viewer.md](viewer.md), [architecture.md](architecture.md) and the README.

Fix with agent uses a separate list, `HANDOFFS` in the same file: how each agent starts interactively on a thread, sent in `initialize` as `handoffs` ([plugins.md](plugins.md#handoffs)). An agent can be in one list and not the other. Amp is only in `HANDOFFS`. dffy itself names no agent.

### Add an annotation source

The comments file is the only source today. An annotation is `dffy_core::annotation::Annotation` (`crates/dffy-core/src/annotation.rs`): an id, its `source`, a path, a side, its lines and an optional `Anchor`. The comments' source is `comments` (`review::model::SOURCE`).

1. Make annotations. Give each one a `source` word of its own. Record its lines with `Anchor::new` on the text they were made on, so placement finds them after edits or reports them lost. Ids are unique within an `AnnotationSet`.
2. Hold them in the set. `AnnotationSet::replace` returns the paths whose annotations changed, and only those are placed again. The viewer's set is the review layer's (`review::model::Comments::annotations`). A second source either joins that set with its own ids, or the set gets an owner that both sources feed. A frontend tells sources apart by `source`, never by reading ids.
3. Place off the main thread. The viewer publishes the set to its pool (`tui::worker::Pool::annotate`). `worker::place_on` places it on each file as the file's diff loads. A placement made for a generation that `AnnotationSet::is_current` no longer accepts is dropped.
4. Show them as blocks. Placements become core blocks, `dffy_core::document::Block` with an `Attach`, passed to `Document::set_blocks`. Their heights come from the host's `Arrange::measure`, and `Document::reveal_annotations` opens the gap lines around them. Search, `Pos` and movement already count block rows, so don't add row arithmetic in the frontend. The viewer's `tui::view::Boxes` maps blocks to comment threads and the text box only. A new source needs its own kind of block there, which defines how it looks.
5. Cover the other frontends. Printed output, `--json` and MCP place through `review::model::Snapshot::on`. `encode` writes `annotations` with their `source` and, for `comments`, their `thread` ([json.md](json.md#annotations)). A new source writes its own `source` word and fields in `crates/dffy/src/encode.rs` and json.md. Consumers that don't know it skip it, so it needs no new `encode::VERSION`.
6. Test placement in `crates/dffy-core/src/annotation/tests.rs` and rows in `crates/dffy-core/src/document/blocks/tests.rs`. Its property tests move through every row of random files with blocks, and check that each block runs unbroken at its measured height and that changes land where they would without blocks. Test each frontend through the binary.
7. Add a benchmark scenario when the source can put many annotations on screen ([below](#add-a-benchmark-scenario)).

### Add a frontend or surface

1. Make it a module of crate `dffy`, under `crates/dffy/src/`, with no `tui` types. Give it a crate of its own only if it earns one.
2. Read the model from dffy-core. For a change set read once, use `print::prepare`'s `Plan` and the core's per-file `File::prepare` steps, as `json` and `mcp` do. For a view a user moves through, use `dffy_core::document::Document`.
3. Get comments from the review layer: `review::model::Snapshot` for a read at one moment, `review::model::Comments` to follow the file as it changes. Say words that hold a `Hint` with the frontend's keys through `review::model::say`.
4. Write JSON through `encode`: its header, file and annotation objects, wrapped in the frontend's envelope. Never spell out the objects again.
5. Drive a running viewer through the control API's client (`control::client`), as `dffy ctl` and the MCP server's viewer tools do. A new message follows the numbered steps at the top of `crates/dffy/src/control/mod.rs`.
6. Add the command in `crates/dffy/src/cli.rs`. A new config key, action, default key or remembered setting needs `task docs`.
7. Write a page for its contract in `docs/`, link it from [docs/README.md](README.md), and add a row to architecture.md's module table.
8. Test through the binary. `crates/dffy/tests/cli.rs` runs it on fixture repositories with an empty config, state and cache. `crates/dffy/tests/mcp.rs` and `crates/dffy/tests/tui.rs` do the same for MCP and the viewer.
9. Add nothing before its first output beyond what printed output already does.

### Add a benchmark scenario

1. Pick the kind. An in-process hot path is a divan benchmark in `crates/dffy/benches/frame.rs` (the viewer) or `crates/dffy-core/benches/engine.rs` (the engine). The `frame` and `engine` groups list benchmarks by name, so a new one is measured with no other change. It shows as only in the checkout until the baseline has it. A whole-program path (opening, settling, a key's frame) is a group of `cargo xtask perf`.
2. For a new group, add its name to `GROUPS` in `crates/xtask/src/ab.rs`, and to `TIMINGS_ONLY` when it has no instruction or allocation counts (the quick tier skips those). Add its arm to the group match in `ab.rs`'s `measure`, returning rows named `<group>/<metric>`. Put the harness in its own module, like `crates/xtask/src/comments.rs`. A scenario that runs its own example binary also needs a row in `EXECUTABLES` there, so both builds produce it.
3. Use a fixed fixture. Generate it at the start of a run from the kernel range in `DFFY_BENCH_REPO`, with the checkout's own dffy, so both builds read the same input on every run.
4. Make a baseline without the feature fail the run with a message, instead of passing a gate it didn't check. `--allow-missing-baseline` runs the group on the checkout alone. A group declares what it needs of a build with a row in `NEEDS` in `ab.rs`: the need in words, and a check of a build's dffy, like the comments group's `comments::opens_in`. The runs and `--full`'s layout bands follow that row.
5. Measure the layout bands. `task perf:full -- --rev <commit> --only <group>` measures the new group's bands alone and adds them to `target/perf/layout.tsv`.
6. Document it in [benchmarks.md](benchmarks.md): a row in the groups table, and a section like [the comments group's](benchmarks.md#the-comments-group) for what the fixture holds and what each metric measures.
7. Run it locally with `task perf -- --rev main --only <group>`, then on the reference Linux machine, where the gate is.

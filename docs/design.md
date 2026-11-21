# Design

The approach a change to dffy follows, for contributors and coding agents. [architecture.md](architecture.md) maps the crates and modules as they are; this page says where a new piece goes and what it must keep true. A plan that can't follow a rule here says which and why before any code is written.

## Where things go

- **dffy-core owns the model.** What changed and what each line is, the rows of each arrangement, positions, navigation, search, blocks among a file's rows, and where annotations land. It has no terminal code, knows nothing of any code host, and doesn't assume which process hosts it: the host runs its work on threads of its own and supplies text measurement and drawing.
- **Frontends are thin over it.** The viewer, the pager, printed output, `--json`, the MCP server and the control API each read the model and present it. What two frontends share sits in a layer between them that has no frontend's types: the review layer (the comments file, its threads, the review session) and the encoder (the one JSON form of the model).
- **When a feature only works by teaching a frontend a special case, the model is missing a concept.** The signs: the viewer counting rows the core doesn't know, a walker of its own, a cache to hide a mismatch. Add the concept to the model so every frontend gets it, instead of patching the view. A frontend decides how things look and how input maps to actions, never what exists.
- **Shared layers say what they mean, not how it looks.** Words that point to an action carry a hint the frontend says with the key it binds there. Text is escaped where it's drawn, and colors and roles are picked there.
- **Layers in dffy-core import downward only**, as architecture.md's layer table lists them.

## Ownership

- **One owner, one write path.** Each piece of state has one owner, and every change to it goes through that owner's write. The comments file is dffy's: a typed comment, `dffy comments apply`, the control API's `comments/add` and a review plugin's operations all go through the same write, under the file's lock.
- **Others go through a protocol.** A party that isn't the owner asks it, through a small versioned protocol, and never touches the state directly. A review plugin returns data and never writes the comments file or calls back into dffy. A control API client asks the viewer, which carries the request out between frames as it would a key.
- **The owner of a fact says it.** What only the host knows (a review's head, the last reviewed head, whether a thread is resolved there) comes from the plugin, and dffy takes it as said rather than working it out.
- **A file with several writers** (the remembered state) has each writer change only its own keys and write back the others as it read them.

## Integrations

- **Plugins are separate processes and data sources.** dffy starts them, asks them, and draws what they answer. They have no screen, never prompt, and run nothing inside dffy: there is no scripting in dffy.
- **Code-host logic lives only in plugins.** dffy-core and the viewer hold no host's names, ids, URLs or rules. A review plugin names its command, its nouns, its events and its labels, and dffy draws them. A host's own fields ride along under the plugin's prefix; dffy keeps them and hands them back unread.
- **More hosts are coming.** Check every message, field and UI concept against GitHub (pending reviews, threads), GitLab (merge requests, draft notes, discussions, approvals), Gerrit (patch sets, drafts, ported comments, votes on several labels) and a review with no host, before adding it. A field only one host can fill is optional with a meaning any host could give it, or it's the plugin's own. The "Designed for" list in [plugins.md](plugins.md#the-review-protocol-version-1) is the checklist.
- **dffy never touches the network.** Plugins do.

## Protocols

- **Versioned.** The version is in the handshake, and in a socket's name where there's a socket, so peers of different versions refuse each other and say which version they speak.
- **Unknown fields are ignored**, both ways, so an optional field comes without a new version.
- **Capabilities are negotiated at start** in a session protocol: what a peer can do is said once, at connect, and the other side never asks for what wasn't offered.
- **Experimental protocols carry one version**, changed in place, with dffy and the implementer in this repository changing together. No compatibility layer while there's a single implementer.
- **One encoding of the model.** `--json`, MCP and the control API share the encoder's objects and add only their envelopes, so their schemas can't drift. A change that breaks a consumer bumps the schema's version.
- **One page is the contract.** Each protocol and format has a page in `docs/` ([plugins.md](plugins.md), [control.md](control.md), [json.md](json.md), [mcp.md](mcp.md), [comments.md](comments.md)), and tests hold the code to it.

## Performance

Speed comes first ([architecture.md](architecture.md) has the priorities and budgets). The reference workload is the Linux kernel's `v6.0..v6.1`.

- **Nothing new on the first-frame path.** A feature's work starts after the first frame (a plugin's start, the control API's registration) or runs only for the rows on screen. What must run before it, such as a review plugin's naming when the arguments could be its command, is cached and capped by a timeout.
- **No per-frame work.** What a frame needs is computed when its input changes and looked up while drawing.
- **No polling.** dffy waits on events (the watcher, a process's output, a socket) or on a deadline its loop sleeps until; idle, nothing wakes. The watcher polls only where the OS can't watch. A plugin that must ask its host owns its cadence and pauses when told you're idle.
- **Compute once per load, off the main thread.** Per-file work (the diff, placement, highlighting, scopes) runs in the host's worker pool once per version of the file, stamped so a result made for an older version is dropped rather than waited for.
- **Measure against `main`.** Before and after, locally, with the perf gate ([benchmarks.md](benchmarks.md#checking-a-change-for-regressions)). The gate that counts runs on the reference Linux machine of [benchmarks.md](benchmarks.md#setup). Each feature path has a benchmark scenario, so later changes are gated on it too.

## User experience

- **Surface problems where the user acts.** A draft that won't post as it is says so in its heading once it's written, not after the post fails.
- **One plain key per action.** A letter or a plain key that every terminal, multiplexer and phone keyboard sends, with no chords that need extended keyboard protocols. A key such a protocol enables is an extra over the plain ones: in the text box `enter` sends, and `alt+enter` and `ctrl+j` start a new line everywhere, so `shift+enter`, which needs the kitty keyboard protocol, only adds to them. Actions have names, and the key tables' keys can be rebound ([keys.md](keys.md) is generated from them).
- **The right-click menu is a shortcut that teaches the keys.** Each entry is something a key does there, shown with its key, and choosing it does what the key does. Nothing lives only in the menu or only on the mouse.
- **Nothing silent.** Every outcome is stated: a result in the status bar, a failure until it's seen, a fallback (lines instead of the rendered view, the line diff instead of the syntax diff) with why.

## Testing

- **Test the contract through its real consumer.** The CLI through the built binary, the viewer in a pseudo-terminal, a protocol through the program on its other side, a wire format through fixtures both sides check.
- **Stub programs are files**, under a `tests/fixtures/stubs/` directory, written into a temporary directory with `include_str!`. No shell script inline in a test.
- **Failing test first** for a change in behavior.
- **One case per behavior.** Add an input only when it takes a new path. Property tests where values round-trip (hunks applied, positions through rows); snapshots for screens.
- **The user's environment stays out.** A test that runs the binary gives it an empty config, state and cache.

## How a change lands

1. **A plan with a verifiable goal**: "this scenario stays within its band", "this test passes", not "make it work". The plan names the files of each step, the order, and how it's validated.
2. **An independent review per phase**, by someone, or an agent, that didn't build it.
3. **The checks** in [AGENTS.md](../AGENTS.md) pass.
4. **Measured before and after** against `main` for anything near startup, scrolling or a hot path. The numbers go in the pull request.
5. **Docs state the contract as it is now.** History and measurements go in commits and pull requests.

## Guides

### Add a review plugin

1. **The program.** An executable named `dffy-review-<name>`, the name lowercase letters, digits, `-` and `_`, found on `PATH`. Any language. In this repository it's a crate under `plugins/<name>` (the workspace takes `plugins/*`), as `plugins/review-github` is.
2. **The protocol.** JSON-RPC 2.0, one message a line, on stdin and stdout ([plugins.md](plugins.md#the-review-protocol-version-1)). Answer `initialize` at once, from nothing but the plugin itself, with [the description](plugins.md#the-description): `protocol`, `command`, `noun` and `nouns`, and the optional `events` and `capabilities`.
3. **The requests.** Map the host onto `list`, `detect`, `prepare` and `pull` (a `session` notification as soon as the commits are in the clone, then `comments/apply` and `progress`), `dryRun` (plans only; it sends nothing), `post` and `doctor`. A request it doesn't have answers `-32601`.
4. **Data out, never state.** Comments go to dffy as `comments/apply` operations. The host's fields go on comments prefixed with the plugin's name, as `github_thread`. Its caches go under a data directory of its own.
5. **Concurrency.** Answer requests on threads of their own, so a `dryRun` answers while a `prepare` downloads. Stop a cancelled request where it can, answering `-32800`.
6. **Watching**, when the host's changes can be seen: `"capabilities":{"watch":true}`, push `head/moved` and `comments/changed` for the review named in `watch`, at a cadence the plugin owns, paused by `idle/changed`.
7. **A concept the protocol lacks** is a protocol change, not a workaround in the plugin. Check it against the hosts under "Designed for", then change [plugins.md](plugins.md), the parsing in `crates/dffy/src/plugin/review.rs`, what dffy does with it in `crates/dffy/src/review/session.rs`, and `plugins/review-github`, in one change.
8. **Tests against the contract.** `crates/dffy/tests/fixtures/review-protocol/` holds a comments file and, in `requests.json`, the requests dffy makes for it. dffy's `requests_carry_what_the_protocol_says_and_never_the_file` (`crates/dffy/src/plugin/review/tests.rs`) and dffy-review-github's `the_harness_carries_what_dffy_carries` (`plugins/review-github/tests/review.rs`) both hold to it. Stub the host's tools as files, as `plugins/review-github/tests/fixtures/stubs/gh.sh` does.
9. **Docs and release.** A page for what it does and keeps (`docs/review-<host>.md`, as [review-github.md](review-github.md)), linked from [docs/README.md](README.md); its builds in `.goreleaser.yaml`, its name in `install.sh`'s `--plugin` list and the Taskfile's `install:plugins`.

### Add an agent recipe

A recipe is how `dffy-agent` runs a coding agent's CLI so an ask can only read, and how it reads what the CLI prints. Each is an entry of `RECIPES` in `plugins/agent/src/agent.rs`.

1. **Check the CLI first**, its latest release: a headless mode that takes the prompt on stdin, the flags that hold it to reading (tools, sandbox, MCP servers, the repository's own config, network), its output, how a run fails, and how it says it isn't logged in. An agent with no read-only mode an ask can rely on gets no recipe; it can still be a command of the user's, as Amp is.
2. **The entry.** A `Recipe`: its `name` (the program on `PATH`, and its name in `initialize`), `args`, the `worktree_flag` when the CLI wants the worktree named, the `mark` its replies carry, what the recipe allows for `doctor` (`said`, and `brief` in a few words for its row), its `login` (`Login::Status` for a status command whose exit 1 means logged out, `Login::Stderr` for the words a failed start prints) and its `log_in` command.
3. **The event reader**, a function beside `claude_event`, `codex_event` and `pi_event` that reads one JSON line: it sets the stream's `answer`, `failure` and `error` by the rule its CLI needs (Claude's final result, Codex's failed turn that outweighs any answer, Pi's last message that outweighs those before it), and returns the progress to show, from the words in the protocol's `progress` row or a new one documented there.
4. **The policy.** Only the latest release is targeted: no version checks or floors, the agent is available when it's on `PATH`, and `--version` runs only for `doctor`. A release that breaks the recipe gets the recipe fixed. The prompt goes on stdin and the agent runs in the worktree; there's no login flow.
5. **A stub.** `plugins/agent/tests/fixtures/stubs/<name>.sh`, as `pi.sh`: it answers `--version`, lists the recipe's flags on `--help` but the one a test puts in `unknown`, writes its arguments, directory and stdin under `$STUB/<name>`, and prints the `answer.jsonl`, `stderr` and `status` a test puts there. Add its directory to `Fixture::new` and a helper that installs it, in `plugins/agent/tests/agent.rs`.
6. **Fixtures.** `plugins/agent/tests/fixtures/<name>-*.jsonl`, shaped on a real run's output: an answer with the progress it goes through, a suggestion, and each way a run fails.
7. **Tests** through the built plugin: its arguments, stdin and directory; the answer; the suggestion; each failure; not logged in; listed when on `PATH`, with its `doctor` rows. Its `not installed` row joins the existing `doctor` tests and `doctor`'s result in `crates/dffy/tests/fixtures/agent-protocol/messages.json`, whose check count `crates/dffy/src/plugin/agent/tests.rs` holds.
8. **Docs.** The recipe, how its answer and failures are read, and what stays possible, in [plugins.md](plugins.md#the-agents-and-their-recipes); its name, progress words and log-in command in the protocol's tables there. The pages that name the agents: the `agent` setting's text in `crates/dffy/src/config.rs` (then `task docs`), [viewer.md](viewer.md), [architecture.md](architecture.md) and the README. Fix with agent has its own list, `HANDOFFS` in the same file: how the agent starts interactively on a thread, which `initialize` describes as its `handoffs` ([plugins.md](plugins.md#handoffs)). An agent can be in one list and not the other, as Amp is only in `HANDOFFS`; dffy names neither.

### Add an annotation source

The comments file is the one source today. An annotation is `dffy_core::annotation::Annotation` (`crates/dffy-core/src/annotation.rs`): an id, its `source`, a path, a side, its lines and an optional `Anchor`. The comments' source is `comments` (`review::model::SOURCE`).

1. **Make annotations.** Give each one a `source` word of its own, and record its lines with `Anchor::new` on the text they were made on, so placement finds them after edits or says they're lost. Ids are unique within an `AnnotationSet`.
2. **Hold them in the set.** `AnnotationSet::replace` returns the paths whose annotations changed, and only those are placed again. The viewer's set is the review layer's (`review::model::Comments::annotations`), so a second source either joins that set with ids of its own, or the set gets an owner both sources feed. A frontend tells sources apart by `source`, never by reading ids.
3. **Place off the main thread.** The viewer publishes the set to its pool (`tui::worker::Pool::annotate`); `worker::place_on` places it on each file as its diff loads, and a placement made for a generation `AnnotationSet::is_current` no longer accepts is dropped.
4. **Show them as blocks.** Placements become core blocks, `dffy_core::document::Block` with an `Attach`, given with `Document::set_blocks`; their heights come from the host's `Arrange::measure`, and `Document::reveal_annotations` opens the gap lines around them. Search, `Pos` and moving count block rows already: don't add row arithmetic in the frontend. The viewer's `tui::view::Boxes` maps blocks to comment threads and the text box only, so a new source needs its own kind of block there, which is how it looks.
5. **The other frontends.** Printed output, `--json` and MCP place through `review::model::Snapshot::on`, and `encode` writes `annotations` with their `source` and, for `comments`, their `thread` ([json.md](json.md#annotations)). A new source writes its own `source` word and fields in `crates/dffy/src/encode.rs` and json.md, which consumers that don't know it skip, so it needs no new `encode::VERSION`.
6. **Tests.** Placement in `crates/dffy-core/src/annotation/tests.rs`; rows in `crates/dffy-core/src/document/blocks/tests.rs`, whose property tests move through every row of random files with blocks and check each block runs unbroken at its measured height, and that changes land where they would without blocks; each frontend through the binary.
7. **A benchmark scenario** when it can put many annotations on screen ([below](#add-a-benchmark-scenario)).

### Add a frontend or surface

1. **A module of crate `dffy`**, under `crates/dffy/src/`, with no `tui` types. A crate of its own only if it earns one.
2. **Read the model from dffy-core.** For a change set read once, `print::prepare`'s `Plan` and the core's per-file `File::prepare` steps, as `json` and `mcp` do. For a view a user moves through, `dffy_core::document::Document`.
3. **Comments from the review layer.** `review::model::Snapshot` for a read at one moment, `review::model::Comments` for one that follows the file as it changes. Words holding a `Hint` are said with the frontend's keys through `review::model::say`.
4. **JSON through `encode`.** Its header, file and annotation objects, wrapped in the frontend's envelope. Never spell the objects again.
5. **Driving a running viewer** goes through the control API's client (`control::client`), as `dffy ctl` and the MCP server's viewer tools do. A new message follows the numbered steps at the top of `crates/dffy/src/control/mod.rs`.
6. **The command** in `crates/dffy/src/cli.rs`. A new config key, action, default key or remembered setting needs `task docs`.
7. **A page for its contract** in `docs/`, linked from [docs/README.md](README.md), and a row in architecture.md's module table.
8. **Tests through the binary**: `crates/dffy/tests/cli.rs` runs it on fixture repositories with an empty config, state and cache; `crates/dffy/tests/mcp.rs` and `crates/dffy/tests/tui.rs` do the same for MCP and the viewer.
9. **Startup.** Nothing before its first output beyond what printed output already does.

### Add a benchmark scenario

1. **Pick the kind.** An in-process hot path is a divan benchmark in `crates/dffy/benches/frame.rs` (the viewer) or `crates/dffy-core/benches/engine.rs` (the engine); the `frame` and `engine` groups list them by name, so a new one is measured without other changes, and shows as only in the checkout until the baseline has it. A whole-program path (opening, settling, a key's frame) is a group of `cargo xtask perf`.
2. **A new group.** Name it in `GROUPS` in `crates/xtask/src/ab.rs`, and in `TIMINGS_ONLY` when it has no instruction or allocation counts (the quick tier leaves it out). Add its arm to the group match in `ab.rs`'s `measure`, returning rows named `<group>/<metric>`, with the harness in a module of its own, as `crates/xtask/src/comments.rs`. A scenario that runs an example binary of its own also needs a row in `EXECUTABLES` there, so both builds produce it.
3. **A fixed fixture.** Generate it at the start of a run from the kernel range in `DFFY_BENCH_REPO`, through the checkout's own dffy, so both builds read the same input on every run.
4. **A baseline without the feature** fails the run, saying so, rather than passing a gate it didn't check; `--allow-missing-baseline` runs the group on the checkout alone. A group says what it needs of a build with a row in `NEEDS` in `ab.rs`: what it needs, in words, and a check of a build's dffy, as the comments group's `comments::opens_in`. The runs and `--full`'s layout bands go by that row.
5. **Layout bands.** `task perf:full -- --rev <commit> --only <group>` measures the new group's bands alone and adds them to `target/perf/layout.tsv`.
6. **Document it** in [benchmarks.md](benchmarks.md): a row in the groups table, and a section like [the comments group's](benchmarks.md#the-comments-group) for what the fixture holds and what each metric measures.
7. **Run it**: locally with `task perf -- --rev main --only <group>`, then on the reference Linux machine, where the gate is.

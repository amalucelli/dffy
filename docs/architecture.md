# Architecture

For contributors and coding agents.

- **Priorities, in order:** performance, usability, developer experience.
- **Reference workload:** the Linux kernel repository (97,000 files; `v6.0..v6.1` changes 13,168 of them).

## Crates

The workspace has two crates, plus `xtask` for the benchmark tooling. The experimental renderer plugins under `plugins/` are programs of their own and are never linked into dffy. Neither crate has a stable public API.

### `dffy-core`

The engine: git access, diffing, highlighting. It has no terminal code and doesn't assume which process hosts it.

| Module | What it owns |
|---|---|
| `git` | Revision resolution (through gix), streaming change enumeration, content loading. |
| `git::spec` | `git diff`-style arguments resolved to the two sides of a diff. |
| `git::tree` | Tree-to-tree and tree-to-index diffs as ordered merge-joins that skip equal subtrees. |
| `git::status` | The working-tree side: a parallel stat scan of the index in index order, and the untracked walk. |
| `git::rename` | Rename detection after enumeration, following git's diffcore-rename so pairs and scores match `git diff -M`. |
| `git::changes` | A diff's file list kept current for watch mode, with the parsed index kept loaded. |
| `git::paths` | Two files or folders compared outside git, behind the same `Repo` handle. Walks both trees in step; a worker per core stats and compares files, and results come back in path order. |
| `diff` | The hunk model, and the line diff: histogram with a Myers fallback. |
| `diff::intraline` | Token-level emphasis inside changed lines. |
| `structural` | The syntax-aware diff: anchoring, per-region matching, the result cache. |
| `highlight` | tree-sitter highlighting of the requested lines; the bundled grammars. |
| `watch` | The file watcher: inotify and FSEvents backends, coalescing, ignore rules, polling. |
| `cache` | The content-addressed disk cache shared by every dffy process. |
| `suggest` | Closest-name suggestions for "did you mean" hints. |

### `dffy`

The `dffy` binary: CLI, printed output, pager mode, the viewer and the watch broker. It's also a library so the benchmarks can reach it.

| Module | What it owns |
|---|---|
| `cli` | clap definitions; inserts the default `diff` subcommand. |
| `config` | The config file, every key with its default, value sources. |
| `state` | Remembered viewer settings in `$XDG_STATE_HOME/dffy/state`. |
| `keys` | Named actions and the three key tables. |
| `docs` | The generated reference pages (`dffy config --markdown`). |
| `theme` | Roles, themes and tints. |
| `output` | Terminal facts, color decisions, stdout or a pager process. |
| `print` | Printed output from the repository, streamed file by file. |
| `no_index` | When two arguments are paths to compare rather than revisions, and what the comparison is called (`git difftool`'s temporary copies included). |
| `pager` | Parsing a unified diff from stdin, the stdin reader, and `--color-only`. |
| `plugin` | Renderer plugins: the `render` setting, finding `dffy-render-<name>` on `PATH`, running one with a timeout, the protocol's JSON. |
| `render` | Layout of diffs into role-tagged rows (split, unified, stat), shared by printed output and the viewer. |
| `term` | The terminal layer: cell grid, raw mode, input parsing, OSC 52. |
| `tui` | The viewer (submodules below). |
| `daemon` | `dffyd`: the broker, its client, and the socket protocol. |
| `doctor` | `dffy doctor`. |
| `bench` | Entry points for the benchmarks. |

The viewer's submodules, under `tui`:

| Submodule | What it owns |
|---|---|
| `app` | State and event handling. |
| `view` | One file's rows. `view::rendered` turns a renderer's blocks into a view. |
| `worker` | Diffs and renderings off the UI thread. |
| `tree`, `picker`, `commands`, `help` | The file tree, the file picker, the command panel, the key list. |
| `chrome` | The frame. |
| `piped` | Diffs from a pipe. |
| `editor` | Opening `$VISUAL` or `$EDITOR` at a line. |

### `plugins/markdown`

`dffy-render-markdown`, the Markdown renderer plugin. It turns pulldown-cmark events into blocks of styled lines and writes the protocol's JSON with serde_json. [plugins.md](plugins.md) has the protocol.

### `xtask`

`cargo xtask`, not published: the performance gate (`bench-baseline`, `bench-noise`, `bench-ab`) and the comparison with other diff tools (`compare`). [benchmarks.md](benchmarks.md) describes both.

## Data flow

The viewer never waits for the whole diff.

1. A change-set thread lists changed files in git's path order and sends them as they're found. For a range that's a tree merge-join; for the working tree, a parallel stat scan of the index, then the untracked walk. The first files paint before the scan finishes.
2. Once the list is complete, the rename pass pairs deletions with additions and updates the list in place.
3. A worker pool diffs a file when it's about to be drawn, then computes emphasis and highlighting for the hunks on screen only. The syntax diff, when on, runs on the files on screen after the line diff has painted. When idle, the pool counts every file's lines for the tree.
4. Rendering touches only the rows drawn. A view's position is anchored to a row of a hunk instead of a screen row, so loading a diff, resizing or switching layout never measures a whole file.
5. Renderer plugins run last, and only when asked. With the rendered view on, a worker sends the file shown to its renderer once its line diff is loaded. The answer's blocks are diffed as units (`diff::unit_hunks`) into an ordinary view (`view::rendered`) that takes the line diff's place. Without a `render` setting, nothing about renderers happens.

### Printed output

- Revisions are resolved before anything is written, so errors never land inside a pager.
- Files then stream one by one.
- With renames on, files from the first addition or deletion on are held until the rename pass has paired them.

### Pager mode

- stdin is parsed in 64KB chunks, with a flush after each.
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

- **Highlighting:** a file side is parsed once, and the highlight query runs only over the lines asked for. Queries compile on first use per language (3–35ms each), beside that first parse. When both sides are 32KB or more they parse on two threads at once, in print mode only while cores are free. Sides over 512KB, or whose parse takes over 200ms, stay plain.
- **Grammars:** Bash, C, C++, CSS, Go, HTML, Java, JavaScript, JSON, Lua, Markdown, Python, Ruby, Rust, TOML, TSX, TypeScript and YAML (tree-sitter, bundled). They're about 15MB of the 20MB binary.

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
- **Lifetime:** the broker exits 60 seconds after its last subscriber, and takes a lock so only one runs per version.
- **Slow subscribers:** each gets its own write queue and is dropped after a one-second stall, so it can't hold up the others or the watcher.
- **Fallback:** a client that can't reach the broker watches by itself. Printed output and the pager never use it.

[watch.md](watch.md) has the user-facing side.

## The renderer

dffy has its own terminal layer (`term`); it uses no TUI library.

- **Cell grid:** the screen is a double-buffered grid of styled cells. Each flush sends only the cells that changed since the last frame.
- **Synchronized output:** every flush is wrapped in `CSI ? 2026 h` / `l`, so terminals that support it never show half a frame.
- **Input:** raw termios and dffy's own parser. It uses no extended keyboard protocols, only input that every terminal and multiplexer passes through.
- **The one query:** the background color (OSC 11), with a 200ms timeout and a dark fallback.
- **Restore:** the terminal is put back on exit, on panic (release builds abort on panic, so a hook does it), on SIGINT/SIGTERM/SIGHUP, and while the editor runs.
- **Layout:** `render` writes role-tagged rows into a `Canvas`. Roles become escape sequences only when a canvas is written to stdout or copied into the grid, so printed output and the viewer share layout code.

## Cache and state

### Disk cache (`$XDG_CACHE_HOME/dffy`)

- **Contents:**
  - rename results;
  - highlight spans and syntax diffs of files of 64KB or more;
  - renderer plugins' answers, keyed by the renderer's program as installed, both sides, the width and the background.
- **Keys:** an entry's name hashes the dffy version, its kind and every input (blob ids, grammar, options), so entries never need invalidating.
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
| First paint, range | 50ms | First 50 paths of `v6.0..v6.1` in 1.8ms, all 13,168 in 29ms; first viewer frame in a pty in 5–7ms |
| First paint, working tree | 50ms | First modified file in 24ms, all tracked files in 51ms, with untracked files 111ms |
| Pager, first output | 20ms | 3.6ms on a 53MB diff; 6.4MB peak memory for the whole diff |
| Print, first output | — | 4.1ms for the range, 30ms for the working tree |
| Frame render | 8ms | 160–244µs for a full 200×60 frame, including the 13,000-file tree |
| Watch refresh | 16ms | About 11ms from the write to the repaint, 8ms of it the debounce |
| Renames on the range | — | 254 of 254 pairs match git, in 33–35ms (git: 194–201ms) |
| Line diff, worst file (1.1MB) | 1.5× imara-diff | 2.4ms, 1.16× imara-diff |
| Syntax diff, five large kernel files | 500ms, 256MB each | 8–267ms in process, at most 100MB per process; `difftastic` fell back to a line diff on all five |

Measured on a 4-vCPU, 15GB Linux VM.

To reproduce:

1. Point `DFFY_BENCH_REPO` at a kernel clone with tags `v6.0` and `v6.1`.
2. Run `task bench`: the divan benchmarks in `crates/dffy-core/benches/engine.rs` and `crates/dffy/benches/frame.rs`.
3. Run `task test:kernel`.

Both skip without `DFFY_BENCH_REPO`.

## Testing

| Command | Runs |
|---|---|
| `task ci` | `fmt:check`, `lint` (clippy, warnings are errors), `test`, `docs:check`, `doc`, `deny`, `machete`, `typos`, in order |
| `task test` | Every unit and integration test with cargo-nextest, then doctests |
| `task test:kernel` | The kernel tests in release mode; needs `DFFY_BENCH_REPO` |
| `task bench:ab` | The current tree against a baseline from `task bench:baseline`, failing on a slowdown over 1% that's consistent across rounds, or an allocation increase (`cargo xtask`, see [benchmarks.md](benchmarks.md#checking-a-change-for-regressions)) |
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
| dffy | gix, tree-sitter with 18 grammars, ignore, rustix, objc2 (macOS), signal-hook, unicode-width, memchr, clap (with clap_complete and clap_mangen), rustc-hash, thiserror |
| `plugins/markdown` | pulldown-cmark, serde_json, unicode-width |
| `xtask` | serde_json, portable-pty, vt100 |

- **gix** is built without `max-performance`, which pulls in an MPL-2.0 crate.
- **JSON:** dffy has no JSON crate; the renderer protocol's JSON is hand-written.
- **Checks:** `task deny` checks advisories, licenses, bans and sources; `task machete` finds unused dependencies.

## Releases

goreleaser builds the release from `.goreleaser.yaml`:

- macOS (x86_64, aarch64) with the native toolchain;
- Linux (x86_64, aarch64) through cargo-zigbuild;
- a Homebrew cask pushed to `amalucelli/homebrew-tap`.

```sh
task snapshot              # build the artifacts locally, no tag, no publish
task release TAG=v0.1.0    # sign and push the tag, then publish with goreleaser
```

`task release` reads a GitHub token from `gh auth token`. goreleaser reads the tag from the remote, so the tag is pushed before it runs.

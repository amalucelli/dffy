# AGENTS.md

dffy is a terminal diff viewer and review tool for git, written in Rust, that stays fast on repositories the size of the Linux kernel.

Read [docs/design.md](docs/design.md) before changing behavior. It has the rules a change follows. [docs/architecture.md](docs/architecture.md) maps the modules.

## Where code goes

- `crates/dffy-core`: the model. Diffs, rows, positions, navigation, search, blocks, annotation placement. No terminal code, no code-host knowledge.
- `crates/dffy/src/review`: the comments file, its threads and the review session. No viewer types.
- `crates/dffy/src/encode.rs`: the one JSON form of the model, for `--json`, MCP and the control API.
- `crates/dffy/src/{print.rs,pager,json.rs,mcp.rs,control}`: frontends, thin over the model and the review layer.
- `crates/dffy/src/tui`: the viewer. How things look and how keys map to actions.
- `crates/dffy/src/plugin`: running plugins, and their protocols on dffy's side.
- `plugins/*`: plugins, each a program of its own. Code-host logic lives only here.
- `crates/xtask`: the perf gate and the comparison with other tools.

## Before calling work done

1. `task fmt:check`
2. `task lint`: clippy on every crate and target with all features. Warnings are errors.
3. Targeted tests for what changed: `cargo nextest run -p <crate> <filter>`. `task test` runs everything.
4. `task docs:check`. After changing a config key, an action, a default key or a remembered setting, run `task docs` first.
5. `task typos`
6. Anything on startup, scrolling, drawing or another hot path: `task perf -- --rev main --only <groups>` with `DFFY_BENCH_REPO` set ([docs/benchmarks.md](docs/benchmarks.md)), and report before and after. The gate that counts runs on the reference Linux machine.

## Rules

- Failing test first when behavior changes.
- Stub programs in tests are files under `tests/fixtures/stubs/`, never shell inline in a test.
- Docs state the current contract. History and measurements go in commits and pull requests.

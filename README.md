<p align="center"><img src="assets/logo.png" width="170" alt="dffy"></p>

<h1 align="center">dffy</h1>

<p align="center"><b>A terminal diff viewer for git, written in Rust.</b></p>

<p align="center">Built for large repositories, up to the size of the Linux kernel.</p>

```sh
dffy                   # everything you haven't committed
dffy main..HEAD        # any revision git understands
```

<p align="center"><img src="assets/screenshot.png" alt="dffy reviewing the Linux v6.0..v6.1 diff"></p>

## Install

```sh
brew install --cask amalucelli/tap/dffy
# or
curl -fsSL https://raw.githubusercontent.com/amalucelli/dffy/main/install.sh | sh
# plus the experimental renderers: … | sh -s -- --plugin markdown --plugin mermaid
```

Then try it in any repository: `dffy HEAD~10`.

## About

`dffy` is a diff viewer for the terminal. Run it in a git repository and it opens a full-screen view of your changes: a file tree on the left, the diff on the right, split or unified. Pipe it, and it prints the diff instead.

It's built for big diffs. Instead of computing the whole diff up front, `dffy` shows the first files as soon as it finds them and works on the rest in the background, so a 13,000-file change opens as fast as a one-line fix.

It fits into how you already work. Every action is a single key, the layout adapts from a wide monitor down to a phone over SSH, and `dffy` can stand in for git's pager, `git add -p` filter and difftool. Settings live in a plain config file you can keep with your dotfiles.

## Why `dffy`

`dffy` builds on tools I like: the terminal review flow of [`lumen`](https://github.com/jnsahaj/lumen), the review UX of [`hunk`](https://hunk.dev), and the rendering of [diffs.com](https://diffs.com) (Pierre's `@pierre/diffs`). None of them fit how I work. `lumen` centers on AI features I didn't want in a diff viewer. `hunk` is TypeScript on a JavaScript runtime, and I wanted a single native binary that stays fast on huge repos. diffs.com runs in the browser. So I built a native terminal diff viewer that works the same on a laptop and over SSH from a phone.

## Features

**Fast**
- ⚡ [Opens the Linux v6.0..v6.1 diff in 9.5ms](docs/benchmarks.md) and loads the rest in the background.
- 👀 [`--watch`](docs/watch.md) repaints as you edit, stage and commit.

**Reading diffs**
- 🪟 [Split or unified](docs/config.md#settings), picked by terminal width.
- 🌈 Syntax highlighting for 30 languages, with [changed words emphasized](docs/viewer.md#modified-lines).
- 🌳 [A syntax-aware diff](docs/architecture.md#the-engine) (`d`) that marks reformatted (`~`) and moved (`»`) code.
- 🔁 Renames detected the way git detects them.

**Navigation**
- 🗂️ [A file tree](docs/viewer.md#panels), a fuzzy file picker and [search across the diff](docs/viewer.md#search).
- 🎯 [Each file opens at its first change](docs/viewer.md#where-a-file-opens), and `n`/`N` jump between changes.
- ⌨️ [Single-key actions](docs/keys.md), with a [command panel](docs/viewer.md#panels) on `space`.

**Git**
- 🧾 [Staged, unstaged and untracked](docs/git.md#staged-unstaged-and-untracked) changes in one view, or [any revision](docs/usage.md#what-to-compare).
- ⚔️ [Conflicted files](docs/git.md#staged-unstaged-and-untracked) from a merge, rebase or cherry-pick, with their conflict markers highlighted.
- 📟 Works as [git's pager](docs/git.md#as-gits-pager), [`git add -p` filter](docs/git.md#git-add--p) and [difftool](docs/git.md#git-difftool).
- 📁 [Compares files and folders](docs/usage.md#comparing-files-and-folders) outside a repository too.

**Anywhere**
- 📱 [Works on phones over SSH](docs/viewer.md#phones-and-small-screens), with plain-letter keys and a frameless layout.
- 🖱️ [Mouse selection](docs/viewer.md#selection-and-copy) that copies over SSH.
- 🎨 [16 themes](docs/themes.md) and a [config file](docs/config.md) for your dotfiles.
- 🧩 [Renderer plugins](docs/plugins.md) (experimental): Markdown diffed as rendered text, and Mermaid flowcharts and sequence diagrams drawn in the terminal.

**Scripts and agents**
- 🧱 [`--json`](docs/json.md): the diff as JSON Lines, with each line's kind and whether it moved or was only reformatted.
- 🤖 [`dffy mcp`](docs/mcp.md): an MCP server that lets coding agents list a change's files and read its diffs a page at a time.

## Usage

```sh
dffy                        # staged, unstaged and untracked changes
dffy --staged               # what's staged (git diff --cached)
dffy HEAD~3                 # a revision against the working tree
dffy main...feature         # changes since the merge base
dffy show HEAD              # a commit with its message
dffy main.. -- src/         # limited to paths
dffy a.txt b.txt            # two files or folders, no repository needed
dffy --watch                # refresh on every change
dffy --syntax v6.0 v6.1     # diff by syntax
dffy --stat main..          # also --name-only, --name-status, --exit-code
dffy --json main..          # JSON Lines for scripts and agents
dffy mcp                    # an MCP server for coding agents
```

With git:

```sh
git config --global core.pager dffy
git config --global interactive.diffFilter "dffy --color-only"
```

In the viewer, `n`/`N` move between changes, `f`/`F` between files, `/` searches, `o` finds a file, `space` lists commands and `q` quits. `~/.config/dffy/config` holds `key = value` lines; `dffy config --default` lists every key with its default.

## Comparison

| | `dffy` | `hunk` | `lumen` | `difftastic` | `delta` | `diff-so-fancy` | `diff` |
|---|:-:|:-:|:-:|:-:|:-:|:-:|:-:|
| Interactive viewer with a file tree | ✅ | ✅ | ✅ | ❌ | ❌ | ❌ | ❌ |
| Watch mode | ✅ | ✅ | ✅ | ❌ | ❌ | ❌ | ❌ |
| git's pager (reads a diff on stdin) | ✅ | ✅ | ❌ | ❌ | ✅ | ✅ | ❌ |
| `git add -p` filter | ✅ | ❌ | ❌ | ❌ | ✅ | ✅ | ❌ |
| Syntax highlighting | ✅ | ✅ | ✅ | ✅ | ✅ | ❌ | ❌ |
| Changed-word emphasis | ✅ | ✅ | ✅ | ✅ | ✅ | 🟡 | ❌ |
| Syntax-aware diff | ✅ | ❌ | ❌ | ✅ | ❌ | ❌ | ❌ |
| Split and unified, picked by width | ✅ | ✅ | ❌ | 🟡 | 🟡 | ❌ | 🟡 |
| Mouse | ✅ | ✅ | ✅ | ❌ | ❌ | ❌ | ❌ |
| Compares files outside a repository | ✅ | 🟡 | ❌ | ✅ | ✅ | 🟡 | ✅ |
| Plugins or extensions | 🟡 | 🟡 | ❌ | ❌ | ❌ | ❌ | ❌ |
| Single native binary | ✅ | 🟡 | ✅ | ✅ | ✅ | ❌ | ✅ |
| AI or agent annotations | ❌ | ✅ | 🟡 | ❌ | ❌ | ❌ | ❌ |
| Other VCSs (Jujutsu, Mercurial, Sapling) | ❌ | ✅ | ✅ | ✅ | ✅ | 🟡 | — |

🟡 means partial support. All seven are open source. GNU diff is GPL-3.0-or-later and the rest are MIT. Checked against `hunk` 0.25.0, `lumen` 2.32.0, `difftastic` 0.72.0, `delta` 0.20.1, `diff-so-fancy` 1.4.14 and GNU diffutils 3.10, with [a source for every cell](docs/benchmarks.md#feature-comparison-sources).

## Benchmarks

Opening Linux v6.0..v6.1 (13,168 changed files, a 48MB patch) in a viewer, time until the first changed line is on screen:

| | `dffy` | `lumen` 2.32.0 | `hunk` 0.25.0 |
|---|--:|--:|--:|
| The v6.0..v6.1 range | 9.5ms | 12.4s | 17.7s |
| The working tree (5 files changed) | 63ms | 1.12s | 790ms |
| As a pager, `git diff v6.0 v6.1 \| …` | 217ms | n/a | 15.3s |
| Memory, opening the range | 90MB | 676MB | 4.26GB |

As git's pager, `dffy` streams the whole 48MB patch in 3.0MB of its own memory, about as fast as plain `git diff`.
## Architecture

`dffy` is written in Rust.

- 📚 [A library at its core](docs/architecture.md#crates), `dffy-core`: the diff model the viewer, printed output, JSON and the MCP server all share.
- 🧮 [Its own diff engine](docs/architecture.md#the-engine): histogram and syntax-aware diffs, renames matching git's.
- 🖼️ [Its own terminal renderer](docs/architecture.md#the-renderer), with no TUI framework. A full frame takes about 200µs.
- 🌊 [Streamed input and output](docs/architecture.md#data-flow), so the first screen doesn't wait for the whole diff.
- 🛰️ [One shared watcher per repository](docs/architecture.md#watcher-and-broker), however many views are open.

## Docs

The [documentation](docs/README.md) covers every key, flag and setting: [installation](docs/install.md), [usage](docs/usage.md), [the viewer](docs/viewer.md), [keys](docs/keys.md), [configuration](docs/config.md), [themes](docs/themes.md), [git integration](docs/git.md), [watch mode](docs/watch.md), [renderer plugins](docs/plugins.md), [JSON output](docs/json.md), [the MCP server](docs/mcp.md), [troubleshooting](docs/troubleshooting.md), [architecture](docs/architecture.md) and [benchmarks](docs/benchmarks.md).

## License

MIT

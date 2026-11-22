# Changelog

The big changes in each release. Plugins and features marked experimental can still change between releases.

## 0.4.0 (2026-11-22)

### Added

- **Ask** (experimental). With the new `dffy-agent` plugin on `PATH`, `a` on a thread asks Claude Code, Codex or Pi about it, read-only, and the answer lands in the thread as the agent's reply, with a suggestion when it proposes a change. `a` on selected lines opens a box for your question first, `a` again after an answer writes a follow-up to the same agent, and a comment you write starting `@claude`, `@codex` or `@pi` asks that agent. The prompt carries the thread, its lines, the changes near them, the file's language and the function they're in. Agents' answers, questions and follow-ups are private, never posted to a code host until `A` on the thread makes them yours. The `agent` setting picks the agent or names a command of yours. See [docs/viewer.md](docs/viewer.md#asking-an-agent) and [docs/plugins.md](docs/plugins.md#agent-plugins).
- **Fix with agent**: `f` on a thread, or `space` then `&` on selected lines with a note you write first, starts your own `claude`, `codex`, `pi` or `amp` in a new terminal tab, pointed at the thread, to change what it asks and reply in it. `space` then `^` sends every open thread of yours the same way.
- **Apply suggestion**: `s` on a thread puts its suggestion in the working tree as an unstaged change, refused when the lines changed since; `s` again puts the old lines back. Also `dffy ctl apply-suggestion`, the control API's `comments/applySuggestion` and the MCP tool `apply_suggestion`.
- **Checkpoints**: `dffy checkpoint`, or `space` then `;`, saves the working tree, and `dffy --since-checkpoint` (`space` then `'`) shows only what changed since, such as an agent's turn. Fix with agent takes one before it starts. The MCP tools `changed_files` and `file_diff` take `since_checkpoint`. See [docs/git.md](docs/git.md#checkpoints).
- **`dffy agents setup`** installs a skill that teaches Claude Code, Codex, Pi and Amp to read changes and answer your threads with dffy's CLI; `dffy agents status` and `remove` check and undo it. See [docs/agents.md](docs/agents.md).
- **Copy as prompt**: `y` on a thread, or `space` then `"`, copies the thread or the selected lines as a prompt for a coding agent. `dffy comments prompt <id>` prints it, `dffy comments prompt --open` every open thread of yours, and the MCP tool `thread_prompt` returns it.
- **Open terminal here**: `space` then `.` opens a new tab at the working tree's root, in tmux, zellij, WezTerm, kitty, Ghostty, iTerm2, Rex, herdr, Terminal.app or Alacritty.
- **Delete resolved threads**: `space` then `-` deletes the resolved threads shown, after a y/n. On a code host's review it deletes only what isn't posted.
- The MCP tool `reply_to_thread` writes a reply in a thread under the MCP client's name, and `dffy comments apply --as` writes as a named agent.
- Threads say where their comments stand: `draft`, `private · A shares` or `branch draft`.

### Changed

- **The command panel** opens on `space` as a quick popup of what applies to what you're on; `space` or `tab` again shows the whole panel, in tabs (Here, View, Files, Review, Git, Copy), laid out like `?` help.
- On a focused thread, `a` asks an agent, `A` makes comments not posted yours (adopt, which was `a`), `s` applies a suggestion and `f` fixes with agent. There they no longer stage (`A`), switch the layout (`s`) or go to the next file (`f`); those keep working everywhere else.
- In the command panel, the theme moved from `a` to `~`.
- In the text box, `enter` sends the comment and `esc` still saves it. A new line is `shift+enter` where the terminal reports it, and `alt+enter` or `ctrl+j` everywhere.
- While you write a new thread, its lines stay marked with a rail in the gutter.
- Threads on files the diff doesn't change open from the comments panel, or with `m` past the diff's last thread: the whole file, as the working tree has it. See [docs/viewer.md](docs/viewer.md#files-outside-the-diff).
- `dffy doctor` runs its checks side by side, in about 0.4s instead of 1s, grouped into sections.

### Performance

- Stepping between search matches that are on most lines is about 7% faster, with up to 21% fewer allocations. Drawing and scrolling are unchanged against 0.3.0 on the reference Linux machine.

## 0.3.0 (2026-11-20)

### Added

- **Review comments** (experimental). Select lines and press `c` to start a thread under them, or `C` for the whole file. Threads take replies, resolves and suggestions, render Markdown, fold, and their text can be selected and copied. `i` opens a Comments panel beside the file tree, and a right-click menu runs the same actions. Comments are kept per branch in a file under the repository's git directory and follow their lines as the code moves. See [docs/comments.md](docs/comments.md).
- **`dffy comments`** lists and filters comments as text, Markdown or JSON, and `dffy comments apply` lets scripts and agents add their own, shown with their author. `--show-comments` adds them to printed output and `--json`, and the MCP server gained `list_comments`.
- **Code host reviews** through review plugins (experimental). With `dffy-review-github`, `dffy pr 123` opens a GitHub pull request with its threads and a Conversation page. Plain `dffy` on a branch with an open PR attaches its comments to your range, and `p` brings in new pushes and comments. Your drafts post as a review, each comment once, even after a retry or a crash. Needs [`gh`](https://cli.github.com), logged in. See [docs/plugins.md](docs/plugins.md).
- **A control API** for a running viewer: `dffy ctl` and the MCP tools `open_in_viewer`, `focus_thread` and `viewer_selection`, limited to your own user. See [docs/control.md](docs/control.md).
- **Staging and discarding lines** from the viewer. A discard refuses to overwrite edits the view hasn't shown yet.
- **Resolving conflicts** in the viewer: keep the current, incoming or both sides, or take the base, then write the file.
- **A sticky scope line** that names the function or block the top of the screen is in, for 10 languages.
- **`paging = always`** (or `--paging=always`) opens the viewer as git's pager even for a diff that fits one screen, without waiting to see whether it fits.

### Changed

- Conflict keys moved to their own `conflict:` table, active only in a conflicted file: `n`/`N` next and previous conflict, `o` current, `t` incoming, `b` both, `a` base, `w` write. In the main table, `O` now opens a review, `R` shows changes since your last review and `E` writes a comment in `$EDITOR`. Rebinds of conflict actions in your config need the `conflict:` prefix.
- Thread actions (reply, edit, delete, resolve, adopt, open on the host) are a remappable `thread:` table.
- The command panel was re-lettered so a letter never means two things. `w` watch, `W` wrap, `I` whitespace, `B` status bar, `f`/`F` next and previous file; search moved to `/`. [docs/keys.md](docs/keys.md) lists them all.
- `--json` annotations carry a `source`.

### Performance

- Drawing, scrolling, search and the file tree are 1–5% faster than 0.2.1 on the reference Linux machine.
- `m` between comment threads is 25–36% faster, and comments add nothing to the first frame.
- The pager's peak memory is about 0.6MB higher, from the bigger binary, at the same speed.

## 0.2.1 (2026-11-17)

### Changed

- The MCP server's docs explain each tool's arguments and paging.

## 0.2.0 (2026-11-16)

### Added

- **`--json`**: the diff as JSON Lines, with each line's kind and whether it moved or was only reformatted.
- **`dffy mcp`**: an MCP server that lets coding agents list a change's files and read its diffs a page at a time.
- **The Mermaid renderer plugin** (experimental), drawing flowcharts and sequence diagrams in the terminal.
- Syntax highlighting for 12 more languages, 30 in all.
- Conflicted files from a merge, rebase or cherry-pick, with their conflict markers highlighted.
- `opt`/`alt`+`left` goes to the previous file.

### Changed

- The diff model moved into the `dffy-core` library, which the viewer, printed output, JSON and the MCP server share.

### Performance

- Scrolling a line is about 55% faster and full frames 14–19% faster, with a cache of line counts and terminal-side scrolling.

## 0.1.0 (2026-11-15)

The first release.

- A full-screen viewer with a file tree, split or unified by terminal width, that opens the Linux v6.0..v6.1 diff in under 10ms and loads the rest in the background.
- Syntax highlighting for 18 languages, with changed words emphasized, and a syntax-aware diff (`d`) that marks reformatted and moved code.
- Renames detected the way git detects them.
- A fuzzy file picker, search across the diff, and `n`/`N` between changes.
- Staged, unstaged and untracked changes in one view, or any revision.
- `--watch` repaints as you edit, stage and commit.
- Works as git's pager, `git add -p` filter and difftool, and compares files and folders outside a repository.
- Plain-letter keys and a frameless layout for phones over SSH, mouse selection that copies over SSH, 16 themes and a config file.
- Renderer plugins (experimental), starting with Markdown diffed as rendered text.

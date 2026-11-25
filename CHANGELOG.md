# Changelog

The big changes in each release. Plugins and features marked experimental can still change between releases.

## 0.5.0 (2026-11-24)

### Added

- **Go to definition**, at either version of the diff, in 18 languages and without a language server. `K` lists the names on a line, or ctrl/alt+click one, and a peek under the line shows its definitions and, below them, the lines using it: `c` keeps only calls, `i` only files in the diff, `s` looks in the other version, all remappable in the `peek:` table. Names are matched and resolved as far as imports, locals and receiver types tell, and several definitions are all listed. The index is kept in the disk cache and lives in a helper process that exits after 10 idle minutes. See [docs/viewer.md](docs/viewer.md#go-to-definition).
- **File view**: `l` shows each file whole at one version, with changed lines marked `┃` and removed lines folded into a row that `x` opens in place. `s` flips the version, and `file-view = true` starts in it. See [docs/viewer.md](docs/viewer.md#file-view).
- **Files outside the diff**: `o` finds any file of the repository at either version (`tab` switches), and `a` in the tree lists them all, with the diff's changes colored among them (`all-files`). See [docs/viewer.md](docs/viewer.md#opening-files).
- **Back and forward**: `backspace` goes back to where the last jump left and `\` forward again. The line a jump lands on is marked `❯` until you move, in the theme's new `landed` style. See [docs/viewer.md](docs/viewer.md#jumping-back).
- The MCP tool `definitions` gives agents the same lookup, and with `references` the lines using a name. See [docs/mcp.md](docs/mcp.md#definitions).
- **Send threads to a running agent** (experimental). With a coding agent's session listening in the working tree, `f` (one thread) and `h` (all your open threads) send to it instead of starting a new agent, and the menu and command panel name it. With several, they go to the one you picked before, else the only one that edited files, else dffy asks. The dffy plugin for Claude Code makes its sessions listen: `claude plugin marketplace add amalucelli/dffy`, then `claude plugin install dffy@dffy`, for sessions started after. Any agent that can run a command in the background and act on its lines can listen with `dffy comments listen --as <name>`, and report its prompts and edits with `dffy comments listening`. After upgrading, run `dffy daemon stop` and reopen your viewers and sessions. See [docs/viewer.md](docs/viewer.md#sending-to-a-running-session) and [docs/agents.md](docs/agents.md#claude-code).
- **`h`** sends every open thread of yours, to a listening session or a new agent, after asking (`confirm_send_open_threads`).
- The control API's `comments/listen`, which waits for threads a viewer hands over, and `comments/listening`, which reports what the waiting session last did. See [docs/control.md](docs/control.md#listening-for-threads).

### Changed

- A hand-over from a code host's review names the review's comments file, so with several reviews open the agent reads the one you sent from.
- Viewers register with dffyd again when it goes, so `dffy ctl` keeps reaching them after `dffy daemon stop`.
- dffyd stays running while an agent's session listens.
- What dffy prints for agents tells your comments from anyone else's: prompts indent comments and their suggestions, `dffy comments --markdown` quotes them, and an author whose name reads as `you` shows as `someone named …`. See [docs/comments.md](docs/comments.md#who-you-is).
- `dffy ctl open` and `comment`, the control API's `view/open` and `comments/add`, and the MCP tool `open_in_viewer` open files the diff doesn't change, whole, with `"listed":false` in the result.
- In the command panel, indicators moved from `K` to `,`.
- The command panel's popup opens with no row highlighted, so `space` again shows the whole panel. Once a row is highlighted, `space` and `enter` run it.
- Files opened outside the diff, those with comments included, are listed in the tree under `Not in this diff`, and `x` in the tree closes one without comments.

### Fixed

- Comments on lines that no longer exist show at the end of their file and stay local.
- Emoji with a variation selector take the same number of columns everywhere.

## 0.4.1 (2026-11-22)

### Changed

- A review plugin on `PATH` is used without setting `review`, when it's the only one installed, and `review = none` turns it off. See [docs/plugins.md](docs/plugins.md#enabling-one).

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
- Commented lines show a rail in the gutter, in their thread's color, instead of a mark on every line. A single line keeps its `◆`, `●` or `○`, and with color off the rail starts on it.
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

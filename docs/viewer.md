# The viewer

On a terminal, dffy opens a viewer showing one file at a time; when its output is piped, it prints instead.

- Nearly every action has a key that needs no ctrl or symbol, so it works from a phone keyboard too. Mouse clicks and taps are shortcuts.
- The status bar names the keys to start from for the pane that has the keys, shortened when the screen is narrow:
  - in the diff, `/ search · tab tree · space commands`;
  - in the tree, `/ filter · tab diff · space commands`.
- The first run shows a one-time tip (`tips = false` turns it off).

## Most used keys

| Key | Action |
| --- | --- |
| `space` | Command panel |
| `?` | Every key in effect, your remaps included |
| `j` `k` | Scroll; `ctrl+d`/`ctrl+u` half a screen, `g`/`G` top and bottom of the file |
| `n` `N` | Next or previous change, on into the next or previous file |
| `f` `F` | Next or previous file |
| `/` | Search the diff, or filter the tree when it has the keys ([Search](#search)) |
| `o` | Find a file by name |
| `x` `X` | 20 more lines of the highlighted `⋯ N unchanged lines` gap / every file whole or changes only |
| `t` `tab` | Show or hide the file tree / move the keys between the tree and the diff |
| `s` | Layout: split, unified, auto |
| `W` `d` `w` | Wrap long lines / syntax diff / watch mode |
| `r` | Rendered view, for file types with a renderer plugin (experimental, [plugins.md](plugins.md)) |
| `v` `y` `Y` | Select lines / copy them / copy the file's path |
| `e` | Open `$VISUAL` or `$EDITOR` at the line |
| `c` `m` `M` | Comment on the selection or the top line / next or previous comment thread ([Comments](#comments)) |
| `z` `Z` `i` | Fold a thread or all of them / list every thread |
| `p` `P` | On a pull request (`dffy pr`): update it in place / post your review ([Pull requests](#pull-requests), experimental) |
| `esc` | Step back: close a panel, end a search or filter, cancel a selection, leave a thread or the tree |
| `q` | Quit |

[keys.md](keys.md) lists every action with its keys, the tree's and the command panel's keys, and how to rebind them with `keybind` lines.

## Panels

- **Command panel (`space`):** every setting with its state and key, grouped, plus a few actions. Toggles without a key of their own (indicators, highlighting, whitespace, line numbers, theme) live here.
  - A row's key runs it at once; `j`/`k` and `enter` run the row under the cursor, and `/` narrows the list.
  - Changing a setting keeps the panel open, so several can be changed in a row. Actions that move somewhere else close it.
  - Settings with values show `Layout ‹ auto ›`. On the row, `h`/`l` or `←`/`→` step back and forward, as does a tap on `‹` or `›`; `enter` and the row's key step forward. On other rows `h`/`l` do nothing; highlighting and line numbers are on `H` and `L`.
  - **Theme:** `enter` or a tap opens a list of every theme, dark and light grouped, the current one marked with `*`. Typing filters it (`cat` finds catppuccin), and the theme under the cursor shows on the screen behind as you move. `enter` keeps it, `esc` goes back to the theme you started from. `←`/`→` on the row and its key `c` step through themes without the list.
- **Keys (`?`):** the keys in effect, in five tabs (Move, Files, View, Copy, Other), one short label each. `tab`, `←`/`→` or `1`–`5` switch tabs, and `/` searches all of them. Settings without a key are in the command panel, and [keys.md](keys.md) has the full list.
- **Picker (`o`):** type part of a path, fuzzily; matches in file names rank before matches in folder names. `enter` or a tap opens the file. It reads a piped diff to the end while open, so it finds files not yet reached.
- **File tree (`t`):** folders collapse, single-child folder chains show as one row, and each file has its status letter and line counts. `enter`/`l` opens, `h` collapses, `u` goes to the parent folder, `v` marks a file viewed (dimmed), and `+`/`-` resize. The tree follows the diff as you move between files.

## Where a file opens

- **First visit:** the view starts three rows above the file's first change, so a whole file (`X`, `context = full`) doesn't open at line 1 with the change far below. This holds however you got there: `f`, `n` past the last change of the file before, the tree, the picker, a tap, `--focus`, or the first file at startup.
- **Change near the top:** when the first change already shows on the first screen from the top, as it usually does with changes only, the file opens at its top.
- **Later visits:** the view is where you left it, and watch mode's refreshes never move it.
- **`g`** goes to the top. **`n`** into a file seen before puts its first change at the top, and **`N`** into any file its last change.

## Search

`/` follows the keys.

- **In the diff,** it searches the diff's text, across files.
  - It's case-insensitive unless the search has a capital letter.
  - Matches are highlighted as they're found, and typing doesn't wait for a large file to be searched.
  - `enter` goes to the first match. `n`/`N` then go between matches (`match 3/17` in the status bar) instead of between changes, and `esc` ends the search.
- **In the tree,** it filters the tree fuzzily. `f`/`F` and `n`/`N` keep to the files the filter keeps, and `esc` clears it.

## Selection and copy

- **Mouse:** drag over code to select it, by characters, on one side of a split. It's copied when you let go, and `y` copies it again. A double click selects a word, a triple click the line, and a drag over line numbers whole lines.
- **Keyboard:** `v` starts a selection at the top line, `j`/`k` extend it, `s` picks the side of a split, and `y` copies.
- Copies hold the code only, unless `copy-decorations = true`.
- Copies go through the terminal (OSC 52), so they work over SSH. Under tmux that needs `set -g set-clipboard on`, which `dffy doctor` checks.
- For the terminal's own selection while dffy has the mouse, hold `shift` while dragging, or press `space` then `M` to hand the mouse back.

[troubleshooting.md](troubleshooting.md#copying-clipboard-and-osc-52) covers terminals that block copies.

## Comments

Review comments live in a comments file ([comments.md](comments.md)): `--comments <file>`, or by default `dffy/comments.json` in the repository's git directory. A piped diff has none.

### Reading

- **Threads** show in a box right under the last line they're on, in the code's column. The top border carries the first comment's author (`you` for your own), how long ago it was written, and `draft` until it's posted; each reply follows a blank row, under a line of its own carrying the same for it. Authors are bold. Long comments wrap. Threads under the same line are a blank row apart. The box is drawn in light lines, dashed while the thread holds a draft and isn't resolved, heavy while it has the focus.
- **The thread's state** follows its first comment's author and age: `resolved`, and `outdated` when the host marks the thread outdated. A suggested change shows as the lines it replaces (`-`) and its own (`+`).
- **Comments on a whole file** sit under the file's first row, marked `on the file`. Comments whose lines weren't found sit under its last row, marked `lines not found`.
- **Split view:** a thread takes its side's half.
- **Marks:** a commented line keeps a mark after its line numbers, on its side of a split: `◆` a thread with a draft in it, `●` posted, `○` resolved. The tree shows each file's comment count beside its line counts, in the color of its most pressing thread.
- **Folded threads** take one row of border: `▸ author · age · state · first line`, then `2 replies` at its right end when it has any. Resolved threads start folded, and so do outdated ones without a draft in them; the rest start whole.
- **`z`** folds the focused thread, or the first on screen, and unfolds it. A tap on a thread's top line (`▾` or `▸`) does the same for that thread. **`Z`** folds every thread, or unfolds them all. A thread you're replying in or editing shows whole.

### Finding

- **`m`** and **`M`** go to the next and previous thread, on into the next or previous file with comments, the way `n` and `N` go between changes. The thread they land on takes the focus.
- **The status bar** counts the comments file's threads and drafts: `18 threads · 2 drafts`.
- **`c` in the tree** (or `C` in the command panel) keeps only the files with comments; `f`/`F` and `n`/`N` keep to them, and `esc` in the tree shows every file again.
- **`i`** lists every thread with its place, state and first line, including those on files not in this diff. `enter` goes to one and focuses it.

### Writing

- **`c`** opens a text box right under the selected lines, or with no selection the line `v` would start on. In a rendered view, the comment goes on the source lines the selection was drawn from.
  - Every key types into it: `enter` starts a new line, and the arrows, `home`, `end`, backspace and delete edit. A paste goes in whole, its line breaks kept.
  - `esc` saves the draft and focuses its thread; an empty box saves nothing, and a draft edited down to nothing is deleted. `ctrl+c` saves it and quits.
  - When the save fails, the box stays open with the text and says why; a second `esc` discards it.
  - `ctrl+e` continues in `$VISUAL` or `$EDITOR`, with the text so far. If the editor fails, the box opens again with it.
- **A focused thread** has a heavy border; `m`, `M` or a tap on it focuses one. While it has the focus:
  - `r` replies in a box at the end of the thread. Replying to an `outdated` thread first says that GitHub collapses outdated threads, so the reply is easy to miss there.
  - `e` edits your last draft in it, in place.
  - `x` resolves the thread, or reopens it.
  - `d`, pressed twice, deletes your last draft in it; when that's its first comment, the whole thread.
  - `o` opens it on its host, when it's posted.
  - `esc` leaves it. Other keys keep their meaning: `r`, `x`, `e`, `d` and `o` act on the thread only while it's focused.
- **Posted comments** (those with a `ref`) belong to the host: only resolving changes them.
- **From a phone:** `ctrl+e` needs ctrl, so the command panel's "Continue in editor" (`space` then `E`) does the same: with a thread focused, it opens your draft there (after `esc` saved the box), or a reply to it; with none, a new comment on the line.

### Placing and reloading

- **Placing:** a comment sits on its lines while their text matches what it was written on, or on the one place nearby where those lines and the two around them match. Files with comments are diffed in the background, so every thread finds its place, or is known to have none.
- **Hidden lines:** a thread on lines a `⋯` separator hides shows under the separator. In a rendered view, a thread shows under the last row drawn from its lines, and threads on rows hidden in a rendered view's gaps don't show; `i` still lists them.
- **Search** doesn't look in comments.
- **Reloading:** dffy checks the file four times a second and shows changes from other programs, such as a review plugin, as they're written. The file's `notice`, when it has one, shows in the status bar.

## Pull requests

Experimental. `dffy pr` reviews a pull request through a review plugin, named with `review = <name>` or `--review <name>` ([plugins.md](plugins.md#review-plugins) has `dffy-review-github`). The plugin runs in the background and has no screen; everything happens in the viewer.

- **Opening:**
  - `dffy pr 6064` says `fetching #6064…` until the PR's commits are in the clone, then shows `<base>...<head>` with the PR's comments file. A PR opened before shows at once. Its threads keep arriving while you read, `fetching threads 100/312` in the status bar.
  - `dffy pr` opens the current branch's PR, or with none, the usual diff with the picker open.
  - Plain `dffy` (no revisions or paths) on a branch with a review plugin set opens as usual and asks the plugin, in the background, for the branch's PR. When there's one, the view switches in place to the PR's base against the working tree (`--merge-base <base>`), with the PR's threads: your fixes not pushed yet show with the PR's changes, and the status bar says `PR #6064 + working tree`. A clean working tree still prints `no changes` without waiting for the answer.
  - Drafts written there are posted on the lines they have at the PR's head; one on lines the head doesn't have yet stays a draft and says so.
  - **The picker** (`space` then `P`, "Open pull request…") lists the PRs on this branch, those waiting on your review and yours, by group. `j`/`k` move, `enter` opens one in place of what's shown, `esc` closes.
- **The status bar** starts with the PR: `PR #6064 · 18 threads · 2 drafts`, with what the plugin is doing while it runs.
- **Keeping up:** every 60 seconds dffy asks the plugin whether the PR's head moved, which costs the host's API nothing. When it has: `PR #6064 updated · new commits · p to update`.
  - **`p`** fetches the head and the threads and shows them in place: you stay where you are, threads move with their lines, and your drafts stay. The note counts the new commits.
- **Since last review** (`space` then `R`) shows only what changed from the head your last review was posted on to the head, and back to the whole PR. It needs a review posted from dffy on that PR.
- **Posting:** **`P`** opens a dialog:
  - what goes: new comments, replies, and the threads resolved or reopened since the last post in this session;
  - what the plugin will do with them, checked in the background as the dialog opens (`checking what can be posted…`), without reaching the host: `12 comments · 1 as a file comment`, then `2 can't be posted and stay drafts · v goes to them`. A draft can't be posted when its lines aren't at the PR's head (changes not pushed yet), its file isn't in the PR, or its thread isn't posted. **`v`** closes the dialog on the first of them, its thread focused, with why in the status bar; `m`/`M` go on through the threads from there. Sending anyway submits the review without them, and they stay drafts;
  - a warning when the PR moved since what's shown: the review goes on the head shown;
  - the event: `p` pending (kept as your draft on the host), `c` comment, `a` approve, `r` request changes; `j`/`k` move too;
  - a summary: `s` (or `tab`) writes in it, `esc` comes back.

  `enter` sends it in the background, `posting 3/12` in the status bar. Once posted, the comments turn to the posted look; `o` opens the review, until another key is pressed. `esc` closes the dialog and keeps the summary for next time.

  When the host refuses drafts while posting, which the check couldn't foresee, the plugin doesn't submit a comment, approval or request for changes without them, and a prompt lists every draft not posted, with why:
  - `s` submits the review without them;
  - `f` goes to the first of them to fix it, and `m`/`M` on through the threads; the review stays pending;
  - `k` (or `esc`) keeps the review pending.

  Afterwards the status bar keeps saying how it ended: `review sent` (with its link for a moment), `approved · 2 comments not posted`, or `review still pending · 2 not posted`.
- **Quitting** stops the plugin, with anything it started.

## Layout

One frame goes around the tree and the diff. Its top and bottom edges are the only chrome, two rows in total.

- **Top edge:** names the file (`modified · staged · src/tui/app.rs · +3 -1`) and, at the right, the hunk or function you're in (`@@ fn render()`, or the function alone with `hunk-headers = false`). Short of room, the function goes first, then the folder, cut from the left; the name and counts stay.
- **Bottom edge:** the status bar. What's compared (`uncommitted`, `v6.0..v6.1`, `stdin`), the position (`3/269 · 1/4 hunk`), the modes and the keys. A note (`copied 3 lines`) takes the modes' place for a moment. `status-bar = top`, or `b` in the command panel, moves it to the top edge.

Under 16 rows or 100 columns (a phone), there's no frame. A title row and a status row are the only chrome, and every other row shows code. Under 100 columns, the tree opens over the diff, and the title becomes a file switcher, `‹ 3/12 name.rs ›`, with tap targets.

### Change marks

- **Indicators:** changed lines get a tinted background on truecolor terminals and a colored bar elsewhere (`indicators`). Classic `+`/`-` signs are used whenever output has no color.
- **Tints:** green, red and, for moved code, purple, mixed into the theme's background. For the default theme they mix into the terminal's own background, which dffy asks the terminal for; `background = dark` or `light` skips the question.

### Modified lines

A removed line and the added line it was edited into (the pairs whose changed words are emphasized) show as one modified change, as in JetBrains IDEs.

- Both lines of an edited pair are in the theme's blue, with only the edited words red on the old line and green on the new. The overview column marks them in blue too.
- Lines wholly removed or added stay red and green. Moved (`»`) and reformatted (`~`) code keep their own looks.
- `modified-lines = added-removed`, or `e` in the command panel, draws edited lines red and green like the rest.
- Printed output follows the config file; `--color-only` always uses red and green.

### Colors

Colors come from the theme, which the command panel's Theme row picks. [themes.md](themes.md) lists the built-in themes and covers writing your own.

### Hidden characters

Code and paths never reach the terminal raw. Control characters show in caret notation (`^[`), invalid UTF-8 as `�`, and bidirectional overrides and isolates (U+202A–U+202E, U+2066–U+2069), which would reorder the text around them, by their code point: `<U+202E>`. A line reads in the order its bytes are in, in the viewer and in printed output.

## Phones and small screens

Most actions have a plain-letter key, and the command panel (`space`) reaches the rest with letters too, so a phone keyboard rarely needs ctrl or its symbol layer. Taps arrive as clicks and swipes as the wheel.

- Under 100 columns, the tree opens over the whole diff instead of beside it, and a tap on a file opens it and closes the tree. The title row's `‹ 3/12 name.rs ›` switcher moves between files with a tap on an arrow and opens the picker with a tap on the name.
- `o` finds a file by typing part of its name, without opening the tree.
- The command panel and the key list take the whole screen on narrow terminals.
- A remembered split layout shows unified there, without forgetting the split.

## What `dffy` compares

Plain `dffy` shows everything you haven't committed, staged, unstaged and untracked, and each file's title says where it stands (`modified · partly staged`). `--staged` and `--unstaged` show one side each. [git.md](git.md#staged-unstaged-and-untracked) has the details, and [usage.md](usage.md#what-to-compare) every way to pick what to compare.

## As git's pager

With `core.pager = dffy`, or `git log -p | dffy`, dffy reads the diff git writes and opens the same viewer, with each commit as a tree entry. Short diffs are printed and input that isn't a diff goes to your pager unchanged. [git.md](git.md) covers the setup, `git add -p` and `diff.external`.

## Settings that stick

The viewer remembers your last layout, wrapping, tree, status bar, indicators, modified lines, syntax diff, rendered view, highlighting, theme, mouse, line numbers, hunk headers, copy decorations, whole files (`X`, saved as `context`), ignored whitespace and watch mode, in `$XDG_STATE_HOME/dffy/state`.

- A flag (`--split`, `-U5`, `-w`, `--watch`...) wins for one run and isn't saved. What was remembered beats the config file.
- Printed output, the pager and `--color-only` ignore the state, so scripts always get the same output.
- Revealed gaps (`x`), the search, the tree's filter, viewed marks, the selection and where you are last only for the session.
- Under 100 columns, a remembered split shows unified and a remembered tree doesn't open over the diff; both stay remembered.
- With watch mode remembered, `dffy` in a repository without changes opens and waits for some instead of printing that there are none.
- `remember = false` turns this off.
- Settings an older dffy doesn't know are kept when it writes the file.

`dffy config` prints every setting with where its value comes from. [config.md](config.md) lists every setting.

## Watch mode

`--watch` (or `w`) refreshes the view as files, the index or refs change.

- Every dffy process watching a repo shares one file watcher through `dffyd`, which starts on first use and exits when idle.
- Changes in directories git ignores, and that hold no tracked files, never cause a refresh. [watch.md](watch.md) covers what's watched, the broker and the Linux watch limit.

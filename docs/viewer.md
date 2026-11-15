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
| `esc` | Step back: close a panel, end a search or filter, cancel a selection, leave the tree |
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
- For the terminal's own selection while dffy has the mouse, hold `shift` while dragging, or press `M` to hand the mouse back.

[troubleshooting.md](troubleshooting.md#copying-clipboard-and-osc-52) covers terminals that block copies.

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

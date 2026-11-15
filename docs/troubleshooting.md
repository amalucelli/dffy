# Troubleshooting

## `dffy doctor`

`dffy doctor` checks git, the repository, file watching, the terminal and multiplexer, copying, color, the config, the theme, the pager, the cache and `dffyd`, and times dffy on the repository. Run it inside a repository, or point it at one with `-C <path>`.

```
✓ git         git version 2.43.0
✓ repository  ~/src/linux · master · 2 staged, 5 unstaged, 1 untracked
✓ watch       6,345 needed here · 159 of 129,194 in use
· speed       repo 1ms · first change 31ms · all 8 changes 127ms · untracked 67ms · largest diff 3ms (mm/vmscan.c)
✓ terminal    TERM=xterm-256color, COLORTERM=truecolor, 160 columns
✓ multiplexer tmux; dffy's one query (the background color) falls back to dark if it isn't relayed
✓ copy        tmux passes copies (y, Y) to the terminal: set-clipboard is on
✓ color       on; the default theme uses the 16 ANSI colors, truecolor themes work too
· config      /home/me/.config/dffy/config (not present; using defaults)
✓ theme       default
✓ pager       less -RFX
✓ cache       /home/me/.cache/dffy: 12.0MB in 40 entries, trimmed at 256.0MB
· dffyd       not running; the first dffy --watch starts it
```

| Mark | Meaning |
|---|---|
| `✓` | Passes |
| `!` | Warning |
| `✗` | Failure |
| `·` | Information |

Problems come with a `hint:` line. dffy exits with 1 when a check fails, and 0 otherwise, warnings included.

`dffy doctor --json` prints the same checks as one JSON object, without color, for bug reports and scripts:

- **`checks`:** a list, each with `name`, `status` (`ok`, `warn`, `fail` or `info`), `detail`, `notes` and `hints`.
- The data behind them, under `git`, `repository`, `watch`, `speed`, `terminal`, `config`, `cache` and `dffyd`.
- Every key is always present, `null` when it doesn't apply.
- Durations are milliseconds as numbers (`"all_changes_ms": 127.4`).

| Check | What it looks at | Warns or fails when |
|---|---|---|
| `git` | `git --version` | git isn't on `PATH` (fails). dffy's own diffs work without git; using it as git's pager doesn't. |
| `repository` | The working tree dffy would read here, its branch (`main`, `main (no commits yet)` or `detached at 80f116f`), and the uncommitted files plain `dffy` lists, counted as `git status` does ([Repository counts](#repository-counts)) | Not in one (information only). |
| `watch` | Linux: the inotify watches this repository takes (one per directory git doesn't ignore, plus `.git` and its refs), against `fs.inotify.max_user_watches` and the watches your processes hold. macOS: notes FSEvents needs no per-directory watches | The repository needs more watches than are free (watches `dffyd` already holds for it count as free), or the limit can't be read. See [watch.md](watch.md#linux-the-inotify-limit). |
| `speed` | How long dffy takes here, along the viewer's own code paths: opening the repository, the first change listed, all of them, the untracked-file walk alone, and the diff of the largest changed text file | Never. See [Speed check limits](#speed-check-limits). |
| `terminal` | `TERM`, `COLORTERM`, `TERM_PROGRAM`, the width | `TERM=dumb`. |
| `multiplexer` | tmux, zellij, screen or herdr, from their environment variables | Never; it notes the background-color query (see [Colors](#colors)). |
| `copy` | tmux's `set-clipboard`, inside tmux only | It isn't `on`, or can't be read. See [Copying](#copying-clipboard-and-osc-52). |
| `color` | Whether stdout gets color, and truecolor support | Never. |
| `config` | The config file and any problems in it | A line was skipped; each one is listed. |
| `theme` | The theme in use | Never; a theme that fails to load falls back to `default` with a warning on every run. |
| `pager` | The pager printed output goes through | Never. |
| `cache` | The disk cache's location, size and budget | It can't be read. |
| `dffyd` | The watch broker, its build, and repositories it polls; brokers of other dffy versions | The broker doesn't answer (a stale socket), or comes from another build of dffy than the one running doctor. See [An old broker](#an-old-broker). |

### Repository counts

As in `git status`, a file staged and then changed again counts as staged and unstaged. A file removed with `git rm --cached` counts once, as a staged deletion.

### Speed check limits

- On a huge repository it stops counting at 50,000 changes, and says so.
- It skips the untracked walk or the largest diff once about a quarter of a second has gone by, and says so.
- The untracked walk lists every directory before reporting, so the 50,000 cap doesn't shorten it.

### An old broker

A broker outlives rebuilds and reinstalls, so an old one keeps running old code. `dffy daemon stop` stops it, and the next `dffy --watch` starts the new one.

## Copying: clipboard and OSC 52

`y`, `Y` and mouse selections copy with an OSC 52 escape sequence that the terminal handles, so copying works over SSH and needs no local clipboard tool. The terminal, and anything between dffy and it, has to pass the sequence on. dffy can't tell whether it did.

- **tmux:** drops OSC 52 unless `set-clipboard` is `on`. `dffy doctor` checks it. Set it for the running server and in `~/.tmux.conf`:
  ```sh
  tmux set -g set-clipboard on
  ```
- **herdr and other multiplexers:** dffy doesn't check them. Copy something with `y` and paste it to find out.
- **Terminals:** some ask before letting a program write the clipboard, some turn it off by default and some don't support it. Look for a clipboard or OSC 52 setting in the terminal's configuration.
- **Size:** terminals cap the sequence at different lengths, so dffy refuses copies over 75KB with a note instead of cutting them short.

Copies hold the code only. `copy-decorations = true` (or `D` in the command panel) adds line numbers and `+`/`-` signs.

## Mouse or native selection

dffy takes mouse input for clicks, the wheel, and drags that select and copy code. While it does, the terminal's own selection doesn't work. To get it back:

- Hold `shift` while dragging. Many terminals select natively then, whatever the program asks for.
- Press `M` to hand the mouse to the terminal, and `M` again to take it back. `mouse = false` in the config starts with it off.

## Colors

- **No color at all:** `NO_COLOR` is set, `--color never` or `color = never` is in effect, or stdout isn't a terminal. `--color always` forces it. Without color dffy marks changes with `+`/`-` signs so the output stays a readable diff.
- **Bars instead of tinted lines:** `indicators = auto` tints changed lines only when `COLORTERM` is `truecolor` or `24bit`, and draws a colored bar otherwise.
  - Terminals that support 24-bit color but don't set `COLORTERM` get bars, and so do sessions over SSH or under a multiplexer that drop it.
  - Fix: set `COLORTERM=truecolor` in the environment, or `indicators = background` in the config.
- **Tints too dark on a light background:** the viewer asks the terminal for its background color (OSC 11) and waits up to 200ms for the answer.
  - Multiplexers often don't pass the question on, and dffy then assumes a dark background.
  - Fix: set `background = light` (or `dark`) to skip the question.
  - Printed output always uses the dark tints unless `background = light` is set.
- **Themes:** see [themes.md](themes.md).

## Watch mode stops refreshing

On Linux the usual cause is the inotify limit. `dffy doctor` shows it; [watch.md](watch.md#linux-the-inotify-limit) explains how to raise it. When the limit runs out, dffy polls once a second instead and says so in the status bar.

`dffy daemon stop` stops a broken `dffyd`. Viewers keep watching on their own, and the next viewer to watch starts a new broker. A broker that doesn't answer at all leaves a stale socket in `$XDG_RUNTIME_DIR/dffy` that can be removed.

## Git shows `difftastic`'s output, or no diff

With `diff.external` set, git hands dffy the external tool's output, which dffy passes through untouched. Use `git diff --no-ext-diff`, or plain `dffy`. See [git.md](git.md#diffexternal).

## `git add -p` says "mismatched output"

`interactive.diffFilter` must be `dffy --color-only`, not plain `dffy`. See [git.md](git.md#git-add--p).

## A garbled terminal after a crash

dffy restores the terminal when it exits, panics or is interrupted.

If it was killed outright (`kill -9`, the terminal's process dying), the terminal can stay in the viewer's mode: no echo, the mouse reporting escape codes, the alternate screen. Type `reset` and press enter, even if you can't see what you type.

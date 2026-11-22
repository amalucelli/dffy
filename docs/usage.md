# Usage

`dffy` shows the changes in a git repository. Run it with no arguments to see everything you haven't committed, or give it revisions the way you would to `git diff`.

```sh
dffy                        # staged, unstaged and untracked changes
dffy main..                 # main against HEAD
dffy show HEAD~2            # a commit with its message
dffy main.. -- src/         # only files under src/
dffy a.txt b.txt            # two files, no repository needed
```

On a terminal it opens the full-screen [viewer](viewer.md). In a pipe it prints the diff.

```
dffy [options] [--show-comments] [<rev>...] [-- <path>...]
dffy [options] --since-checkpoint[=<n>] [-- <path>...]
dffy [options] [--no-index] <path> <path>
dffy --json [--show-comments] [options] [<rev>...] [-- <path>...]
dffy show [<rev>] [-- <path>...]
dffy config [--default]
dffy comments [--json | --markdown] [--session] [--open | --resolved] [--path <path>] [--branch <name> | --all-branches]
dffy comments apply [--as <name>] | comments import <file> | comments rm --resolved [--branch <name> | --all-branches]
dffy comments prompt [<id> | --open [--json]]
dffy <review-command> [<number>]    # e.g. dffy pr, with a review plugin set
dffy mcp
dffy ctl views | open <path>[:<line>[:old]] | focus <id> | selection | state | comment <path>:<line> <text> | apply-suggestion <id> | schema
dffy agents setup [--agent <agent>] [--project] [--dry-run] [--force] | agents status|remove [--agent <agent>] [--project] | agents show
dffy checkpoint
dffy themes | doctor [--json | --verbose] | daemon status|stop | completions <shell>
```

Without a subcommand dffy runs `diff`, so `dffy main..HEAD` is `dffy diff main..HEAD`. A revision named like a subcommand needs the explicit form: `dffy diff config`.

## What to compare

| Command | Compares |
|---|---|
| `dffy` | `HEAD` with the working tree: staged, unstaged and untracked changes together |
| `dffy --staged` (`--cached`) | `HEAD` with the index, as `git diff --cached` |
| `dffy --unstaged` | The index with the working tree, as plain `git diff`: no staged changes, no untracked files |
| `dffy --all` | Everything uncommitted, as plain `dffy`. Use it when `default-diff = unstaged` is set |
| `dffy <rev>` | `<rev>` with the working tree |
| `dffy --staged <rev>` | `<rev>` with the index |
| `dffy <a> <b>`, `dffy <a>..<b>` | `<a>` with `<b>` |
| `dffy <a>...<b>` | The merge base of `<a>` and `<b>` with `<b>` |
| `dffy <rev>^!` | The first parent of `<rev>` with `<rev>` |
| `dffy show <rev>` | Same as `<rev>^!`, plus the commit header |
| `dffy a.txt b.txt`, `dffy old/ new/` | Two files or folders, see [Comparing files and folders](#comparing-files-and-folders) |
| `dffy --since-checkpoint` | The latest checkpoint with the working tree: only what changed since `dffy checkpoint`, or since Fix with agent started the agent |
| `dffy --since-checkpoint=<n>` | Checkpoint `<n>`, the number `dffy checkpoint` printed, with the working tree |

Revisions use git's syntax: branch and tag names, short ids, `HEAD~3`, `@{u}` and so on. An empty side of a range means `HEAD`, so `dffy main..` compares `main` with `HEAD`. More than two revisions is an error, and so is `--staged` with two.

| Option | Effect |
|---|---|
| `--merge-base` | The old side becomes the merge base of the revision and `HEAD`, or of both revisions. Needs at least one revision. |
| `--since-checkpoint` | Takes no revisions, `--staged`, `--unstaged` or `--merge-base`. Fails with `no checkpoint in this working tree` before the first one, and with `no checkpoint 2 …` once that one was pruned. Lines shown this way can't be staged in the viewer. [git.md](git.md#checkpoints) has what a checkpoint holds. |
| `-R` | Swaps the two sides. |

Before the first commit, `HEAD` reads as the empty tree ([git.md](git.md#repositories-without-commits)).

An unknown revision exits with the closest ref or subcommand name as a hint:

```
$ dffy mainn
error: unknown revision 'mainn'
hint: did you mean 'main'?
$ dffy README.md
error: unknown revision 'README.md'
hint: 'README.md' is a path; put paths after --, as in: dffy -- README.md
```

[git.md](git.md#staged-unstaged-and-untracked) shows how staging appears in the viewer and how `default-diff` changes plain `dffy`.

## Paths

Paths go after `--` and limit the diff to files under them:

```sh
dffy main.. -- src/ tests/
dffy show HEAD -- crates/dffy/src/cli.rs
```

A path is relative to the current directory and matches whole path components. `src` matches `src/lib.rs` and `src/li` matches nothing. Globs such as `'*.rs'` aren't supported.

`--focus <path>` limits printed output to files under that path. The viewer still lists every file and opens at the first one under it.

## Comparing files and folders

```sh
dffy a.txt b.txt                  # two files
dffy old/ new/                    # two folders, file by file
dffy notes.md backup/             # notes.md with backup/notes.md, as diff does
curl -s "$URL" | dffy - local.json  # stdin as one side
dffy --no-index main dev          # files named like revisions
```

```
$ dffy --paging=never old.txt new.txt
modified: new.txt
─────────────────
@@ -1,3 +1,3 @@
1 1 │ a
2   │-b
  2 │+B
3 3 │ c
```

No repository is needed. Highlighting, `--syntax`, `--stat`, `--name-status`, `--exit-code`, `-R`, `--watch`, themes, keys and remembered settings all work. The status line names the two sides, as in `a.txt → b.txt`.

dffy decides whether two arguments are paths the way `git diff --no-index` does:

| Arguments | Compared as |
|---|---|
| Two existing files or folders, outside a repository | paths |
| Two paths, either of them outside the working tree | paths |
| Two paths inside the working tree, neither a revision | paths |
| Two paths inside the working tree, both also revisions | revisions, as git prefers |
| Two paths inside the working tree, one also a revision | an error: `'main' is both a file and a revision; use --no-index to compare files` |
| `--no-index <path> <path>` | paths, always |
| `-- <path> <path>` inside the working tree | pathspecs that limit the diff (see [Paths](#paths)) |

`-` reads one side from stdin. `/dev/null` stands for a missing file, so the other side shows as added or deleted.

Two files make one entry. It's named after the second file, or the first when the second is stdin, and its language comes from that name. A file and a folder compare the file with the file of the same name inside the folder.

Two folders are walked as `diff -r` walks them:

- Files pair by their path inside each folder. They appear in path order as the walk finds them, so the first changes of a large tree show before the walk ends.
- A file on one side only is added or deleted. Renames between the folders are paired as in a repository, unless `--no-renames` is given.
- Nothing is left out by default: dot files, `.git` and what `.gitignore` lists are all compared. Symlinks are followed.

| Option | Effect |
|---|---|
| `--exclude <glob>` | Leave out paths matching a gitignore-style pattern, relative to the folders. `*.o` and `target/` match at any depth, `/build` only at the top. Repeatable. |
| `--gitignore` | Leave out what each folder's `.gitignore` files list, and `.git` directories. |

`--watch` (or `w`) watches the compared files and folders from dffy itself, not through `dffyd`. A change in a folder re-compares only what changed.

[git.md](git.md#git-difftool) sets dffy up as `git difftool`.

## A diff on stdin

```sh
git diff | dffy
dffy < change.patch
dffy - < change.patch
```

With no revisions, dffy reads a pipe or a file on stdin as a diff. `-` alone reads stdin explicitly and can't be combined with revisions. Next to a file, `-` is that file's other side instead.

dffy never reads a terminal, `/dev/null` or a socket on stdin, so it doesn't hang in scripts and CI. It also ignores stdin when given flags that need a repository, such as `--staged`, `--stat`, `--show-comments` or paths after `--`.

A diff read this way has only its hunks, so the viewer can't show more context, diff by syntax or highlight code. [git.md](git.md#piped-input) covers how dffy handles piped input on a terminal.

## Viewer or printed output

dffy opens the viewer when stdout is a terminal. It prints instead when:

- `--stat`, `--name-only`, `--name-status`, `--exit-code` or `--json` is given.
- `--paging=never` is given, or `paging = never` is set.
- There's nothing to show and the viewer wouldn't be watching. dffy prints `no changes` and exits. For `--staged` and `--unstaged` it says what the other side holds: `no staged changes (2 unstaged, 1 untracked; run dffy to see them)`. With `--watch` the viewer opens and waits.

Printed output on a terminal goes through a pager: `$DFFY_PAGER`, then the `pager` setting, then `$PAGER`, then `less -RFX`. A pager of `cat`, or an empty one, means no pager. To a pipe or a file dffy writes the plain diff, so `dffy main.. | head` and `dffy > out.diff` work.

## Output options

These work with `diff` and `show`, in the viewer and in printed output. Each has a config key in [config.md](config.md), and the flag wins for that run.

| Option | Effect |
|---|---|
| `--split`, `--unified` | Side-by-side or unified layout. The default, `view = auto`, splits on terminals at least `split-min-width` (140) columns wide. |
| `-U <n>`, `-U<n>`, `--unified=<n>` | Lines of context around each change. `full` (`-Ufull`, `--unified=full`) shows the whole file with the changes inline. Bare `--unified` only picks the layout. |
| `-w`, `--ignore-all-space` | Ignore whitespace when comparing lines. |
| `-b`, `--ignore-space-change` | Ignore changes in the amount of whitespace. |
| `--no-renames` | Don't pair deleted and added files as renames. Printed output then streams without waiting for the rename pass. |
| `--syntax`, `--no-syntax` | Diff code by syntax: emphasize only changed tokens, dim code that was only reformatted (`~`) and mark code that moved (`»`). Languages without a bundled grammar keep the line diff. |
| `--highlight`, `--no-highlight` | Syntax highlighting on or off. Files over 512KB are never highlighted. |
| `--wrap`, `--no-wrap` | Wrap long lines, or cut them at the edge. |
| `--watch` | `diff` only. The viewer refreshes as files, the index or refs change ([watch.md](watch.md)). Printed output warns and prints once. |

Binary files, and files of 64MB or more, get a one-line summary instead of a diff. Content that isn't UTF-8 is shown lossily.

## Summaries and exit status

```
$ dffy --stat HEAD^!
 Taskfile.yml             |  5 +++++
 crates/xtask/src/bump.rs | 46 +++++++++++++++++++++++++++++++---------------
 2 files changed, 36 insertions(+), 15 deletions(-)
```

| Option | Prints |
|---|---|
| `--stat` | A diffstat, as `git diff --stat` |
| `--name-only` | Changed paths |
| `--name-status` | Changed paths with a status letter: `A` added, `D` deleted, `M` modified, `R` renamed, `T` type changed, `U` unmerged (still conflicted), `?` untracked |
| `--exit-code` | The diff, then exits with 1 if there were differences and 0 if not |

Only one of the three summaries can be given. `--exit-code` is for `diff` only and combines with them:

```sh
dffy --exit-code --stat main..   # prints the stat, exits 1 if main.. has changes
```

## JSON output

```sh
dffy --json main..
dffy --json --syntax --staged -- src/
```

`--json` writes the diff as JSON Lines: a header, an object per file with its hunks and lines, then an end object. It takes the same revisions, paths and diff options as printed output and is never colored or paged.

- `--syntax` adds moved and reformatted marks, and classifies files whose changes are only formatting or moves.
- `--highlight-spans` adds each line's syntax highlighting.
- With `--stat`, `--name-only` or `--name-status`, file objects have no hunks: paths, status, counts and flags only.

[json.md](json.md) has the schema. `dffy mcp` serves the same objects to coding agents, a page at a time ([mcp.md](mcp.md)).

## Review comments in printed output

```sh
dffy --show-comments main..
dffy --json --show-comments main..
dffy --comments pr.json --show-comments main...feature
```

The viewer always shows review comments ([viewer.md](viewer.md#comments)). Printed output and `--json` show them only with `--show-comments`, so output without the flag is the same whether a comments file exists or not. On a terminal the viewer still opens. Add `--paging=never` to print.

The threads come from `--comments <file>`, or with `--session` from the review session the viewer has open ([comments.md](comments.md#the-session-pointer)), else from the repository's local file. dffy only reads the file. It shows threads on the current branch or on no branch ([comments.md](comments.md#branches)), and leaves out general comments, which aren't on a file.

Each thread is placed by its anchor, as in the viewer ([comments.md](comments.md#anchors)), and printed as a box under the row of its last line:

- A thread on a line no hunk shows goes under the header of the gap that hides it, or after the file's last row. Its border names the line.
- A comment on the whole file goes under the file's header.
- A thread whose lines are gone goes at the end of its file, with the lines it was written on.

The box's top border holds the author, age and state (`resolved`, `outdated`, `pending`, `on the file`, `line 9 not found`). Below it is each comment's text, with Markdown shown as plain text and wrapped to the output's width (80 columns off a terminal). A suggested change shows as `-` and `+` lines. With `--json`, each file object gets an `annotations` array instead ([json.md](json.md#annotations)).

`--show-comments` doesn't combine with `--stat`, `--name-only` or `--name-status`, which have no lines to put threads under. It reads the repository and leaves stdin unread, and with `-` it's an error, so it doesn't work in git's pager setup. A file that `-w` or `-b` finds unchanged isn't printed, and neither are its threads.

## `--color-only`

```sh
git diff | dffy --color-only
```

Colors a diff read from stdin and changes nothing else: one output line per input line, with the same text. It's for git's `interactive.diffFilter` ([git.md](git.md#git-add--p)). It never opens the viewer or a pager, takes no revisions, and refuses a terminal on stdin.

## Subcommands

| Command | What it does |
|---|---|
| `diff` | Changes between the working tree, the index and revisions. The default. |
| `show [<rev>]` | A commit against its first parent, with the commit header. `<rev>` defaults to `HEAD`. A root commit is compared with the empty tree. |
| `comments` | Lists the review comments as threads, prints them as Markdown or JSON, and changes them with `apply`, `import`, `rm --resolved` and `prompt`. See [comments.md](comments.md#reading-dffy-comments). |
| `<review command> [<id>]` | Experimental. Opens one of a code host's reviews in the viewer, such as `dffy pr 123` with `dffy-review-github`. The plugin names the command, and it exists only while there's a review plugin: the only one on `PATH`, or the one `review` or `--review` names. Without an id it opens the current branch's review, or a list to pick from. Needs a terminal. Without such a plugin, `pr` is a revision like any other. See [viewer.md](viewer.md#code-host-reviews) and [plugins.md](plugins.md#the-review-command). |
| `config` | The effective settings, each marked with where its value comes from. `--default` prints every key with its documentation and default. See [config.md](config.md). |
| `themes` | The available themes, with a preview. `--dump <name>` prints a built-in theme as a file to copy and edit. See [themes.md](themes.md). |
| `checkpoint` | Takes a checkpoint of the working tree and prints its number, for `--since-checkpoint=<n>`. See [git.md](git.md#checkpoints). |
| `doctor` | Checks what dffy depends on and times it on the repository. `--verbose` prints every check on its own line, `--json` prints one object. See [troubleshooting.md](troubleshooting.md#dffy-doctor). |
| `mcp` | A Model Context Protocol server on stdin and stdout, for coding agents. Register it with `claude mcp add dffy -- dffy mcp`. See [mcp.md](mcp.md). |
| `ctl` | Drives a running viewer: `ctl open src/lib.rs:42` shows a line, `ctl focus <id>` shows a thread, `ctl selection` prints what's selected. It talks to the viewer in this working tree, or the one `--view <id>` names. See [control.md](control.md). |
| `agents` | `setup` installs a skill that teaches Claude Code, Codex, Pi and Amp to use dffy. `status` checks it, `remove` deletes it, and `show` prints the guide it points to. See [agents.md](agents.md). |
| `daemon status`, `daemon stop` | Inspect or stop the watch broker. See [watch.md](watch.md#dffyd). |
| `completions <shell>` | A completion script for `bash`, `zsh`, `fish`, `elvish` or `powershell`. See [install.md](install.md#shell-completions). |

## Global options

These go before or after the subcommand.

| Option | Effect |
|---|---|
| `-C <path>` | Run as if dffy started in `<path>`. |
| `--color auto\|always\|never` | When to color output. `auto` colors terminals and follows `NO_COLOR` and `CLICOLOR_FORCE`. |
| `--paging auto\|always\|never` | `never` writes straight to the terminal and keeps the viewer from opening. `always` opens the viewer for a piped diff that fits one screen, which `auto` prints ([git.md](git.md#piped-input)). |
| `--theme <name>` | A built-in theme or a file in the config's `themes/` directory. See [themes.md](themes.md). |
| `--comments <file>` | The review comments file, relative to `-C`'s directory. The default is `dffy/comments.json` in the git directory the repository's worktrees share. See [comments.md](comments.md#where-the-file-is). |
| `--review <name>` | The review plugin `dffy-review-<name>` for this run, over the `review` setting. It adds the plugin's command, such as `dffy pr`. `auto` uses the only one on `PATH` and `none` uses none. See [plugins.md](plugins.md#review-plugins). |
| `-h`, `--help` | Help. `dffy diff --help` lists the diff options. |
| `-V`, `--version` | The version. |

## Exit codes

| Code | When |
|---|---|
| 0 | Success, including when the reader of the output goes away (`dffy main.. \| head`). |
| 1 | `--exit-code` found differences, or `dffy doctor` found a failing check. |
| 128 | Usage errors, unknown revisions, not a repository, and other errors, as git uses. |
| 130 | Ctrl-C while a pager was showing the output. |

## Environment

| Variable | Effect |
|---|---|
| `NO_COLOR` | With `--color auto`, turns color off. |
| `CLICOLOR_FORCE` | With `--color auto`, turns color on even when stdout isn't a terminal, unless it's `0`. |
| `COLORTERM` | `truecolor` or `24bit` makes `indicators = auto` tint changed lines. Otherwise they get a bar. |
| `DFFY_PAGER`, `PAGER` | The pager for printed output, see [above](#viewer-or-printed-output). `LESS` defaults to `FRX` when unset. |
| `VISUAL`, `EDITOR` | The editor `e` opens, in that order, then `vi`. |
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR` | Where the config, remembered state, cache and dffyd's sockets live. See [config.md](config.md), [watch.md](watch.md#dffyd) and [control.md](control.md). |
| `COLUMNS` | The width for printed output when neither stdout nor stderr is a terminal. |

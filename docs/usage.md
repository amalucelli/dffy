# Usage

```
dffy [options] [<rev>...] [-- <path>...]
dffy [options] [--no-index] <path> <path>
dffy --json [options] [<rev>...] [-- <path>...]
dffy show [<rev>] [-- <path>...]
dffy config [--default]
dffy comments [--json] | comments apply
dffy <review-command> [<number>]    # e.g. dffy pr, with a review plugin set
dffy mcp
dffy themes | doctor | daemon status|stop | completions <shell>
```

Without a subcommand dffy runs `diff`, so `dffy main..HEAD` is `dffy diff main..HEAD`. A revision with the same name as a subcommand needs the explicit form: `dffy diff config`, `dffy diff comments`.

## Viewer or printed output

dffy opens the full-screen viewer when stdout is a terminal, unless one of these applies:

- `--stat`, `--name-only`, `--name-status`, `--exit-code` or `--json` is given;
- `--paging=never` is given (or `paging = never` is set);
- there's nothing to show and the viewer wouldn't be watching. dffy then prints `no changes` and exits. For `--staged` and `--unstaged` it prints what the other side holds instead: `no staged changes (2 unstaged, 1 untracked; run dffy to see them)`. With `--watch`, the viewer opens and waits for changes.

Otherwise dffy prints.

- **On a terminal,** printed output goes through a pager: `$DFFY_PAGER`, then the `pager` setting, then `$PAGER`, then `less -RFX`. A pager of `cat` or an empty one means no pager.
- **To a pipe or a file,** dffy writes the diff with nothing interactive, so `dffy main.. | head` and `dffy > out.diff` work.

[viewer.md](viewer.md) covers the viewer, and [git.md](git.md) covers dffy as git's pager.

## Subcommands

| Command | What it does |
|---|---|
| `diff` | Changes between the working tree, the index and revisions. The default. |
| `show [<rev>]` | A commit against its first parent, with the commit header. `<rev>` defaults to `HEAD`; a root commit is compared with the empty tree. |
| `comments` | The review comments, as threads. `--json` prints the comments file; `comments apply` changes it with a JSON list of operations on stdin. See [comments.md](comments.md). |
| `<review command> [<id>]` | Experimental, and only with a review plugin set (`review` or `--review`) that describes itself: the plugin's command, named by the plugin, e.g. `dffy pr` for dffy-review-github (a GitLab plugin could name it `mr`). One of the code host's reviews in the viewer: `<base>...<head>` with its review threads, which you answer and post from there. Without an id, the current branch's review, or a list to pick from. It needs a terminal. Without such a plugin, `pr` is a revision like any other. See [viewer.md](viewer.md#code-host-reviews) and [plugins.md](plugins.md#the-review-command). |
| `config` | The effective settings, each marked with where its value comes from. `--default` prints every key with its documentation and default instead. See [config.md](config.md). |
| `themes` | The available themes, with a preview. `--dump <name>` prints a built-in theme as a theme file to copy and edit. See [themes.md](themes.md). |
| `doctor` | Checks git, the repository, the watches it needs, the terminal, the config, the cache and `dffyd`, and times dffy on the repository; `--json` prints it all as one object. See [troubleshooting.md](troubleshooting.md#dffy-doctor). |
| `mcp` | A Model Context Protocol server on stdin and stdout, for coding agents: tools that list changed files and return diffs as the objects of `--json`, a page at a time. Register it with `claude mcp add dffy -- dffy mcp`. See [mcp.md](mcp.md). |
| `daemon status`, `daemon stop` | Inspect or stop the watch broker. See [watch.md](watch.md#dffyd). |
| `completions <shell>` | A completion script for `bash`, `zsh`, `fish`, `elvish` or `powershell`. See [install.md](install.md#shell-completions). |

## What to compare

| Command | Compares |
|---|---|
| `dffy` | `HEAD` with the working tree: staged, unstaged and untracked changes together |
| `dffy --staged` (`--cached`) | `HEAD` with the index, as `git diff --cached` |
| `dffy --unstaged` | The index with the working tree, as plain `git diff`: no staged changes, no untracked files |
| `dffy --all` | Everything uncommitted, as plain `dffy` does by default; for when `default-diff = unstaged` |
| `dffy <rev>` | `<rev>` with the working tree |
| `dffy --staged <rev>` | `<rev>` with the index |
| `dffy <a> <b>`, `dffy <a>..<b>` | `<a>` with `<b>` |
| `dffy <a>...<b>` | The merge base of `<a>` and `<b>` with `<b>` |
| `dffy <rev>^!` | The first parent of `<rev>` with `<rev>` |
| `dffy show <rev>` | Same as `<rev>^!`, plus the commit header |
| `dffy a.txt b.txt`, `dffy old/ new/` | Two files or folders, with or without a repository; see [Comparing files and folders](#comparing-files-and-folders) |

Revisions use git's syntax: branch and tag names, short ids, `HEAD^1`, `HEAD~3`, `@{u}` and so on. An empty side of a range means `HEAD`, so `dffy main..` compares `main` with `HEAD`. More than two revisions, or `--staged` with two, is an error.

- **`--merge-base`:** with one revision, the old side becomes the merge base of it and `HEAD`; with two, the merge base of both. It needs at least one revision.
- **`-R`:** swaps the two sides.
- **Unborn `HEAD`:** before the first commit, `HEAD` reads as the empty tree, so `dffy` shows every file as added and `dffy --staged` shows what's staged. Revisions that name `HEAD` (`dffy HEAD`, `dffy show`) fail with `unknown revision 'HEAD'`, as git's do.
- **Typos:** an unknown revision exits with an error and the closest ref or subcommand name: `hint: did you mean 'main'?`. A revision that's really a path gets `hint: 'README.md' is a path; put paths after --, as in: dffy -- README.md`.

[git.md](git.md#staged-unstaged-and-untracked) explains how staging shows in the viewer and how `default-diff` changes plain `dffy`.

## Paths

Paths go after `--` and limit the diff to files under them:

```sh
dffy main.. -- src/ tests/
dffy show HEAD -- crates/dffy/src/cli.rs
```

A path is relative to the current directory and matches whole path components: `src` matches `src/lib.rs`, `src/li` matches nothing. Globs such as `'*.rs'` aren't supported.

`--focus <path>` limits printed output to files under it. The viewer shows every file and opens at the first one under it.

## A diff on stdin

```sh
git diff | dffy
dffy < change.patch
dffy - < change.patch
```

- **When stdin is read:** with no revisions, a pipe or a file on stdin is read as a diff.
- **`-` alone** reads stdin explicitly and can't be combined with revisions. Next to a file, `-` is that file's other side instead (see [Comparing files and folders](#comparing-files-and-folders)).
- **Never read:** a terminal, `/dev/null` or a socket on stdin, so dffy run from scripts and CI doesn't wait on it.
- **Ignored:** with flags that only make sense against a repository (`--staged`, `--stat`, paths after `--` and so on).

A diff read this way has only its hunks. `x`/`X` can't reveal more lines, the syntax diff isn't available and code isn't highlighted. When output is a terminal, input that isn't a diff goes to the pager unchanged; [git.md](git.md#piped-input) has the details.

## Comparing files and folders

```sh
dffy a.txt b.txt                  # two files
dffy old/ new/                    # two folders, file by file
dffy notes.md backup/             # notes.md with backup/notes.md, as diff does
curl -s "$URL" | dffy - local.json  # stdin as one side
dffy --no-index main dev          # files named like revisions
```

No repository is needed. Everything else works as it does on a repository, in the viewer and in printed output, including:

- highlighting by file extension, `--syntax`;
- `--stat`, `--name-status`, `--exit-code`, `-R`, `--watch`;
- remembered settings, themes and keys.

The status line names the two sides, as in `a.txt → b.txt`.

dffy decides whether two arguments are paths the way `git diff --no-index` does:

| Arguments | Compared as |
|---|---|
| Two existing files or folders, outside a repository | paths |
| Two paths, either of them outside the working tree | paths |
| Two paths inside the working tree, neither a revision | paths |
| Two paths inside the working tree, both also revisions | revisions, as git prefers |
| Two paths inside the working tree, one also a revision | an error: `'main' is both a file and a revision; use --no-index to compare files` |
| `--no-index <path> <path>` | paths, always |
| `-- <path> <path>` inside the working tree | pathspecs that limit the diff, as in git (see [Paths](#paths)) |

`-` reads one side from stdin. `/dev/null` stands for a missing file, so the other side shows as added or deleted.

**Two files** make one entry, named after the second file (the first when the second is stdin). Its language comes from that name.

**Two folders** are walked as `diff -r` walks them:

- **Pairing:** files pair by their path inside each folder and come in path order as they're found, so the first changes of a large tree show before the walk ends.
- **One side only:** a file on one side only is added or deleted. Renames between the two folders are paired as in a repository (`--no-renames` turns that off).
- **Large files:** binary files and files of 64MB or more get the one-line summary.
- **Nothing left out by default:** dot files, `.git` and what `.gitignore` lists are all compared. Symlinks are followed.

| Option | Effect |
|---|---|
| `--exclude <glob>` | Leave out paths matching a gitignore-style pattern, relative to the folders: `*.o` and `target/` match at any depth, `/build` only at the top. Repeatable. |
| `--gitignore` | Leave out what each folder's `.gitignore` files list, and `.git` directories. |

**A file and a folder:** the file is compared with the file of the same name inside the folder.

**Watching:** `--watch` (or `w`) watches the compared files and folders from dffy itself, not through `dffyd`. A change in a folder re-compares only what changed.

[git.md](git.md#git-difftool) shows how to use dffy as `git difftool`.

## Output options

These work with `diff` and `show`, in the viewer and in printed output.

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

Each of these has a config key, and the flag wins for that run. [config.md](config.md) lists the keys.

`diff` also takes `--watch`: the viewer refreshes as files, the index or refs change ([watch.md](watch.md)). Printed output warns and prints once.

Binary files, and files of 64MB or more, get a one-line summary instead of a diff. Content that isn't UTF-8 is shown lossily.

## Summaries and exit status

| Option | Prints |
|---|---|
| `--stat` | A diffstat, as `git diff --stat` |
| `--name-only` | Changed paths |
| `--name-status` | Changed paths with a status letter: `A` added, `D` deleted, `M` modified, `R` renamed, `T` type changed, `U` unmerged (still conflicted), `?` untracked |
| `--exit-code` | The diff, then exits with 1 if there were differences and 0 if not |

The three summaries exclude each other. `--exit-code` is for `diff` only and combines with them:

```sh
dffy --exit-code --stat main..   # prints the stat, exits 1 if main.. has changes
```

## JSON output

```sh
dffy --json main..
dffy --json --syntax --staged -- src/
```

`--json` writes the diff as JSON Lines for scripts, CI and agents: a header, an object per file with its hunks and lines, then an end object. It takes the same revisions, paths and diff options as printed output, and is never colored or paged. `--syntax` adds moved and reformatted marks and classifies files whose changes are only formatting or moves. `--highlight-spans` adds each line's syntax highlighting. With `--stat`, `--name-only` or `--name-status`, file objects come without hunks: paths, status, counts and flags. [json.md](json.md) is the schema. `dffy mcp` serves the same objects to coding agents, paged ([mcp.md](mcp.md)).

## `--color-only`

```sh
git diff | dffy --color-only
```

Colors a diff read from stdin and changes nothing else: one output line per input line, with the same text. It's for git's `interactive.diffFilter` (see [git.md](git.md#git-add--p)).

It never opens the viewer or a pager, takes no revisions, and refuses a terminal on stdin.

## Global options

These can go before or after the subcommand.

| Option | Effect |
|---|---|
| `-C <path>` | Run as if dffy started in `<path>`. |
| `--color auto\|always\|never` | When to color output. `auto` colors terminals, and follows `NO_COLOR` and `CLICOLOR_FORCE`. |
| `--paging auto\|always\|never` | `never` writes straight to the terminal and also keeps the viewer from opening. `always` opens the viewer for a piped diff that fits one screen, which `auto` prints; see [git.md](git.md#piped-input). |
| `--theme <name>` | A built-in theme or a file in the config's `themes/` directory. See [themes.md](themes.md). |
| `--comments <file>` | The review comments file, relative to `-C`'s directory; the default is `dffy/comments.json` in the repository's git directory. See [comments.md](comments.md#where-the-file-is). |
| `--review <name>` | The review plugin, `dffy-review-<name>`, over the `review` setting; it adds the plugin's command (`dffy pr`) for the run. See [plugins.md](plugins.md#review-plugins). |
| `-h`, `--help` | Help; `dffy diff --help` lists the diff options. |
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
| `CLICOLOR_FORCE` | With `--color auto`, turns color on even when stdout isn't a terminal (unless it's `0`). |
| `COLORTERM` | `truecolor` or `24bit` makes `indicators = auto` tint changed lines; otherwise they get a bar. |
| `DFFY_PAGER`, `PAGER` | The pager for printed output; see [above](#viewer-or-printed-output). `LESS` defaults to `FRX` when unset. |
| `VISUAL`, `EDITOR` | The editor `e` opens, in that order, then `vi`. |
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR` | Where the config, remembered state, cache and broker socket live. See [config.md](config.md) and [watch.md](watch.md#dffyd). |
| `COLUMNS` | The width for printed output when neither stdout nor stderr is a terminal. |

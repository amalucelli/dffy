# dffy with git

## As git's pager

Try it for one command first:

```sh
git -c core.pager=dffy diff
git -c core.pager=dffy log -p
```

Then set it for every repository:

```sh
git config --global core.pager dffy
```

git then pipes `git diff`, `git show` and `git log -p` into dffy, which opens the same viewer it uses for its own diffs. [Piped input](#piped-input) covers how dffy handles the input.

## `git add -p`

```sh
git config --global interactive.diffFilter "dffy --color-only"
```

git runs the filter on each diff it shows in `git add -p` and pairs the filter's output with its own lines one for one. Hunk splitting and selection go by those lines. If a filter adds, drops or joins a line, git stops with `mismatched output from interactive.diffFilter`.

`--color-only` turns each input line into exactly one line with the same text. It removes git's escape sequences and adds dffy's colors, without syntax highlighting.

Plain `dffy` (or `dffy --paging=never`) as the filter fails, because its rendering adds separators and rewrites file headers.

## `git difftool`

```sh
git config --global difftool.dffy.cmd 'dffy "$LOCAL" "$REMOTE"'
git config --global difftool.prompt false
git difftool --tool=dffy            # or set diff.tool dffy and run git difftool
git difftool --tool=dffy -d main    # every file at once, as two folders
```

git hands the tool two files at a time: a temporary copy of each side (`/tmp/git-blob-XXXXXX/main.rs`), or the file itself for the working-tree side. dffy compares them as it compares any two files ([usage.md](usage.md#comparing-files-and-folders)).

- **Name:** the working-tree path when one side is in the working tree, otherwise the copy's file name. Highlighting follows that name.
- **Status line:** the file and which of how many it is (`src/main.rs · 1/3`).
- **`q`** moves on to the next file.

git doesn't pass the file's path in the repository to the tool. To name every file by it, export `MERGED` in the command, and dffy uses it:

```sh
git config --global difftool.dffy.cmd 'MERGED="$MERGED" dffy "$LOCAL" "$REMOTE"'
```

With `--dir-diff` (`-d`), git writes both sides into two temporary folders, `left/` and `right/`, and runs the tool once on them. dffy shows them as `left/ → right/` with every changed file in the tree. The working-tree side holds links to your files, which dffy follows.

## `diff.external`

If `diff.external` is set, for example to `difftastic`, git writes that tool's output instead of a diff. dffy doesn't recognize it as a diff and passes it to the pager unchanged. Two ways around it:

```sh
git diff --no-ext-diff     # git writes a plain diff for this command
dffy                       # dffy reads the repository itself and ignores diff.external
```

`dffy doctor` doesn't check for this; `git config --get diff.external` shows whether it's set.

## Piped input

When stdin is a pipe and stdout is a terminal (git's pager setup, or `git log -p | dffy`), dffy first looks at the start of the input:

- **Not a diff** (`git log`, `git blame`, `git branch`): passed through to the pager byte for byte, so it looks as it would without dffy.
- **A diff that fits one screen:** printed, the way `less -F` does, and dffy exits. With `--paging=always` (or `paging = always`), the viewer opens instead.
- **Anything longer:** the viewer opens, reading keys from the terminal.

Timing:

- **Slow writers:** input still undecided after half a second (`git log -S`) is passed through.
- **A diff still trickling in** after a quarter second opens the viewer without waiting to see whether it would fit. With `paging = always` there is no such wait: the viewer opens as soon as the input is recognized as a diff.

In the viewer:

- For `git log -p` and `git show`, each commit is a tree entry with its files beneath it and its message in a header view.
- dffy reads at most 128 entries past the one shown and keeps file contents in a temporary file, so an endless `git log -p` holds git back instead of filling memory.
- A piped diff has only the lines git wrote. `x`/`X` can't reveal more, the syntax diff isn't available, and code isn't highlighted.
- Remembered settings apply as they do to the viewer, but printed output never takes them.

With `--paging=never`, or without a terminal, the diff is printed instead.

## Staged, unstaged and untracked

Plain `dffy` compares `HEAD` with the working tree and adds untracked files, so `git add` never makes a change disappear from view. Each file's title in the viewer says where it stands:

| Title | Meaning |
|---|---|
| `new file · staged` | Added to the index, not committed |
| `modified · unstaged` | Changed in the working tree only |
| `modified · staged` | Changed in the index, the working tree matches it |
| `modified · partly staged` | Some of the change is staged, some isn't |
| `untracked file` | Not in the index |
| `conflicted` | A merge, rebase or cherry-pick stopped on it: shown as the working-tree file, conflict markers highlighted, against `HEAD` |

The file tree and the picker show only the status letter, `U` for a conflicted file.

A conflicted file is listed once, as `U`, by `dffy`, `--staged`, `--unstaged` and `dffy <rev>`, even where `git diff` names it twice or by its content (`A`, `M`). With `--unstaged` it's compared with our side of the conflict, as `git diff` does.

`--staged` (`git diff --cached`) and `--unstaged` (`git diff`) show one side each. `default-diff = unstaged` in the [config](config.md) makes plain `dffy` behave like plain `git diff`, and `dffy --all` brings back everything for one run. In printed output, untracked files get an `untracked file:` header.

## Repositories without commits

Before the first commit, dffy treats `HEAD` as the empty tree: `dffy` shows every file as added (staged or untracked), and `dffy --staged` shows what's staged. Anything that names `HEAD` explicitly fails, the same way it does in git:

```
$ dffy show
error: unknown revision 'HEAD'
$ git log
fatal: your current branch 'main' does not have any commits yet
```

So `git log`, `git show` and dffy as their pager have nothing to show until there's a commit.

## Line endings and filters

Working-tree files are compared and shown as git would store them. Line-ending conversion (`core.autocrlf`, `core.eol`, `text` and `eol` in `.gitattributes`), `ident`, `working-tree-encoding` and clean filters (Git LFS, `filter.<name>.clean` and `.process`) are applied first, so a fresh checkout of a repository with `* text=auto eol=crlf` shows no changes.

- Filters run only for files whose size or timestamps differ from the index, the same files git reads.
- When a filter fails (its program isn't installed, say), the file is shown as it is on disk, with a note saying so. Its diff can then show changes git wouldn't.
- A file whose line endings changed and nothing else is listed as modified, as git lists it, and its diff is empty.

## Symbolic links in paths

A tracked file under a directory that has been replaced by a symbolic link is shown as deleted, and the link as untracked, as git shows them. dffy never reads through the link, and `e` doesn't open the file.

## What dffy doesn't read

dffy reads the repository with its own code, not by running git.

- **Paths after `--`** are prefixes, not git pathspecs with globs.

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

The viewer stages, unstages and discards hunks and picked lines of these diffs itself: `A`, `U` and `D` ([viewer.md](viewer.md#staging)). It builds the patch from its own diff of the index, `HEAD` and the working tree as they are when you press the key, and hands it to `git apply` (`--cached` for the index, `-R` to take changes back out), which keeps the index's cached trees and the working tree's filters right. A discard is worked out the same way before it asks, so its question counts only the lines that differ from the index, and it's refused when a picked working-tree line changed since the diff was shown.

Conflicts are resolved from the viewer too ([Merge conflicts](#merge-conflicts)).

## Checkpoints

A checkpoint is the working tree as plain `dffy` lists it, kept so a later diff shows only what changed since: an agent's turn, say ([viewer.md](viewer.md#checkpoints)). `dffy checkpoint` takes one and prints its number; Fix with agent and `space` then `;` in the viewer take one too.

- **What it holds:** every file plain `dffy` would compare: tracked files as they are in the working tree, staged or not, through the same filters as a diff ([Line endings and filters](#line-endings-and-filters)), untracked files git doesn't ignore, and nothing of files deleted. Untracked nested repositories are left out, and so is a working-tree file too large to diff (64 MiB or more): a tracked one keeps `HEAD`'s version in the checkpoint and an untracked one isn't in it, so a large file isn't copied into the repository's objects on every checkpoint, and it lists as changed since one for as long as it differs from `HEAD`.
- **Where it's kept:** a commit, its tree those files, its parent `HEAD` when it was taken, under `refs/worktree/dffy/checkpoints/<n>`, numbered from 1 in each worktree. git keeps `refs/worktree/` apart for each worktree, and no default refspec fetches or pushes it. Each new checkpoint removes all but the last 5. `git diff refs/worktree/dffy/checkpoints/3^ refs/worktree/dffy/checkpoints/3` shows what was uncommitted then.
- **What it doesn't touch:** the index, `HEAD` and the working tree. It writes objects and its ref, as dffy itself, so it needs no git identity.
- **Since a checkpoint:** `dffy --since-checkpoint` compares the latest with the working tree, `--since-checkpoint=3` checkpoint 3 ([usage.md](usage.md#what-to-compare)). A file untracked then and now is compared with its checkpointed content; one untracked only now shows as untracked.

`--staged` (`git diff --cached`) and `--unstaged` (`git diff`) show one side each. `default-diff = unstaged` in the [config](config.md) makes plain `dffy` behave like plain `git diff`, and `dffy --all` brings back everything for one run. In printed output, untracked files get an `untracked file:` header.

## Merge conflicts

In a conflicted file shown as its working-tree file (plain `dffy`, `--unstaged`, `dffy <rev>`), the viewer resolves each conflict to a side, both, or the base ([viewer.md](viewer.md#merge-conflicts) has the keys). It's git-level: nothing depends on how the merge started or where the branches come from.

- **Conflicts:** read from the markers git wrote in the file: `<<<<<<<`, `=======` and `>>>>>>>`, and `|||||||` before the base's lines with `merge.conflictStyle` `diff3` or `zdiff3`. A marker is seven characters at the start of a line, then the line's end or a space and a label (none for `=======`); a `\r` before the newline doesn't count, so CRLF files read the same. A marker line outside a conflict, or a `<<<<<<<` nothing closes, is the file's own text. Markers of another length (the `conflict-marker-size` attribute) aren't read.
- **Lines that look like markers:** where a side's own text has a marker-like line, such as a Markdown heading underlined with seven `=`, the markers read more than one way. The index's sides of the conflict (stages 1, 2 and 3) settle it: the first reading whose every side is a run of lines of that side in the index wins, and with none, the first `=======` and the first `>>>>>>>` after it.
- **Writing:** the file is read again from disk, its bytes as they are, line endings included, and must still have the conflicts the choices were made on; otherwise nothing is written and the status bar says the conflicts changed. Each chosen conflict is replaced by the lines it keeps, and one without a choice keeps its markers. When the last conflict ends the file and its `>>>>>>>` line has no newline, the resolved file's last line has none either. The file is written beside itself and renamed over it, keeping its permissions. A file that became a symbolic link or anything but a regular file since is refused, so nothing is written through a link.
- **`git add`:** offered once no markers are left, and run as `git --literal-pathspecs add -- <path>`, which marks the conflict resolved in the index. The list is then read again.
- **Not by conflict:** a file deleted on one side and changed on the other (said with the side that deleted it), a binary file, a symbolic link or submodule, a file too large to diff and one shown as on disk because its filter failed. git resolves those: `git checkout --ours` or `--theirs`, `git add`, `git rm`.

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

dffy reads the repository with its own code, not by running git. Staging from the viewer is the exception: `git apply` writes it. A [checkpoint](#checkpoints) is written with dffy's own code too.

- **Paths after `--`** are prefixes, not git pathspecs with globs.

# dffy with git

dffy can stand in for git's pager, the `git add -p` filter and `git difftool`. It also reads the repository directly, and shows staging, conflicts and filtered files the way git sees them.

```sh
git config --global core.pager dffy
```

## As git's pager

To try it for one command without changing your config:

```sh
git -c core.pager=dffy diff
git -c core.pager=dffy log -p
```

With `core.pager` set, git pipes `git diff`, `git show` and `git log -p` into dffy, which opens the same viewer it uses for its own diffs. [Piped input](#piped-input) covers what dffy does with the input.

## `git add -p`

```sh
git config --global interactive.diffFilter "dffy --color-only"
```

git runs the filter on each diff `git add -p` shows and pairs the output with its own lines one for one. If the filter adds, drops or joins a line, git stops with `mismatched output from interactive.diffFilter`.

`--color-only` keeps that pairing. Each input line becomes exactly one output line with the same text, with git's colors replaced by dffy's and no syntax highlighting. Plain `dffy` as the filter fails, because it adds separators and rewrites file headers.

## `git difftool`

```sh
git config --global difftool.dffy.cmd 'dffy "$LOCAL" "$REMOTE"'
git config --global difftool.prompt false
git difftool --tool=dffy            # or set diff.tool dffy and run git difftool
git difftool --tool=dffy -d main    # every file at once, as two folders
```

git hands the tool two files at a time: a temporary copy of each side (`/tmp/git-blob-XXXXXX/main.rs`), or the file itself for the working-tree side. dffy compares them as any two files ([usage.md](usage.md#comparing-files-and-folders)).

- The file is named after its working-tree path when one side is in the working tree, otherwise after the copy. Highlighting follows that name.
- The status line shows the file and its place in the list (`src/main.rs · 1/3`).
- `q` moves on to the next file.

git doesn't pass the file's path in the repository to the tool. To name every file by it, export `MERGED` in the command:

```sh
git config --global difftool.dffy.cmd 'MERGED="$MERGED" dffy "$LOCAL" "$REMOTE"'
```

With `--dir-diff` (`-d`), git writes both sides into two temporary folders and runs the tool once. dffy shows them as `left/ → right/` with every changed file in the tree. The working-tree side holds links to your files, which dffy follows.

## `diff.external`

When `diff.external` is set, for example to `difftastic`, git writes that tool's output instead of a diff. dffy doesn't recognize it as a diff and passes it to the pager unchanged. Either of these gets you a diff:

```sh
git diff --no-ext-diff     # git writes a plain diff for this command
dffy                       # dffy reads the repository itself and ignores diff.external
```

`dffy doctor` doesn't check for this. `git config --get diff.external` shows whether it's set.

## Piped input

When stdin is a pipe and stdout is a terminal, as in git's pager setup or `git log -p | dffy`, dffy looks at the start of the input first:

| Input | What dffy does |
|---|---|
| Not a diff (`git log`, `git blame`, `git branch`) | Passes it to the pager byte for byte, so it looks as it would without dffy. |
| A diff that fits one screen | Prints it and exits, as `less -F` does. With `--paging=always` or `paging = always` the viewer opens instead. |
| A longer diff | Opens the viewer, reading keys from the terminal. |
| Nothing decided after half a second (`git log -S`) | Passes it through. |
| A diff still arriving after a quarter second | Opens the viewer without waiting to see whether it fits. With `paging = always` the viewer opens as soon as the input looks like a diff. |

In the viewer:

- With `git log -p` and `git show`, each commit is a tree entry with its files beneath it and its message in a header view.
- dffy reads at most 128 entries past the one shown and keeps file contents in a temporary file. An endless `git log -p` waits on dffy instead of filling memory.
- A piped diff has only the lines git wrote. `x`/`X` can't reveal more, the syntax diff isn't available, and code isn't highlighted.
- Remembered settings apply in the viewer. Printed output never uses them.

With `--paging=never`, or without a terminal, dffy prints the diff.

## Staged, unstaged and untracked

Plain `dffy` compares `HEAD` with the working tree and adds untracked files, so `git add` never makes a change disappear from view. Each file's title in the viewer says where it stands:

| Title | Meaning |
|---|---|
| `new file · staged` | Added to the index, not committed |
| `modified · unstaged` | Changed in the working tree only |
| `modified · staged` | Changed in the index, and the working tree matches it |
| `modified · partly staged` | Some of the change is staged, some isn't |
| `untracked file` | Not in the index |
| `conflicted` | A merge, rebase or cherry-pick stopped on it. Shown as the working-tree file against `HEAD`, with conflict markers highlighted |

The file tree and the picker show only the status letter, `U` for a conflicted file. A conflicted file is listed once as `U` by `dffy`, `--staged`, `--unstaged` and `dffy <rev>`, even where `git diff` lists it twice or as `A` or `M`. With `--unstaged` it's compared with our side of the conflict, as `git diff` does.

`--staged` (`git diff --cached`) and `--unstaged` (`git diff`) show one side each. `default-diff = unstaged` in the [config](config.md) makes plain `dffy` behave like plain `git diff`, and `dffy --all` brings back everything for one run. In printed output, untracked files get an `untracked file:` header.

The viewer stages, unstages and discards hunks and selected lines with `A`, `U` and `D` ([viewer.md](viewer.md#staging)). The patch comes from the index, `HEAD` and the working tree as they are when you press the key, and `git apply` writes it. A discard asks first and counts only the lines that differ from the index. It's refused when a selected working-tree line changed since the diff was shown.

## Checkpoints

A checkpoint saves the working tree as plain `dffy` lists it. A later diff can then show only what changed since, such as an agent's turn ([viewer.md](viewer.md#checkpoints)).

```sh
dffy checkpoint             # prints its number
dffy --since-checkpoint     # changes since the latest one
dffy --since-checkpoint=3   # changes since checkpoint 3
```

Handing threads to an agent takes one too, and so does the command panel's checkpoint action.

A checkpoint holds every file plain `dffy` would compare: tracked files as they are in the working tree, staged or not, through the same filters as a diff ([Line endings and filters](#line-endings-and-filters)), and untracked files git doesn't ignore. Deleted files and untracked nested repositories aren't in it. A file of 64 MiB or more isn't copied: a tracked one keeps `HEAD`'s version in the checkpoint, an untracked one is left out, and either lists as changed for as long as it differs from `HEAD`.

Since a checkpoint, a file untracked then and now is compared with its saved content. A file untracked only now shows as untracked.

Taking one doesn't touch the index, `HEAD` or the working tree. dffy writes a commit whose parent is the current `HEAD`, under `refs/worktree/dffy/checkpoints/<n>`, numbered from 1 in each worktree. It writes the objects and the ref itself, so it needs no git identity. git keeps `refs/worktree/` separate per worktree, and no default refspec fetches or pushes it. Each new checkpoint removes all but the last 5. To see what was uncommitted at checkpoint 3:

```sh
git diff refs/worktree/dffy/checkpoints/3^ refs/worktree/dffy/checkpoints/3
```

## Merge conflicts

In a conflicted file shown as its working-tree file (plain `dffy`, `--unstaged`, `dffy <rev>`), the viewer resolves each conflict to one side, both, or the base. [viewer.md](viewer.md#merge-conflicts) has the keys. It works from the file and the index alone, however the merge started.

dffy reads the markers git wrote: `<<<<<<<`, `=======`, `>>>>>>>`, and `|||||||` before the base's lines when `merge.conflictStyle` is `diff3` or `zdiff3`. A marker is seven characters at the start of a line, followed by the line's end or a space and a label (no label for `=======`). CRLF files read the same. A marker line outside a conflict, or a `<<<<<<<` nothing closes, is ordinary text. Markers of another length (the `conflict-marker-size` attribute) aren't read.

A side's own text can hold a marker-like line, such as a Markdown heading underlined with seven `=`. dffy then picks the reading that matches the index's three sides of the conflict. With no match it takes the first `=======` and the first `>>>>>>>` after it.

When you write the file, dffy reads it again and writes only if it still has the conflicts you chose on. Otherwise the status bar says the conflicts changed. Each resolved conflict becomes the lines it keeps, and one without a choice keeps its markers. Line endings, a missing final newline and the file's permissions are kept. The file is written beside itself and renamed over it. A file that became a symbolic link or anything but a regular file is refused.

Once no markers are left, dffy offers to run `git --literal-pathspecs add -- <path>`, which marks the conflict resolved, and then reads the file list again.

The viewer doesn't resolve a file deleted on one side and changed on the other (it names the side that deleted it), a binary file, a symbolic link or submodule, a file too large to diff, or one shown as on disk because its filter failed. Resolve those with git: `git checkout --ours` or `--theirs`, `git add`, `git rm`.

## Repositories without commits

Before the first commit, dffy treats `HEAD` as the empty tree. `dffy` shows every file as added (staged or untracked), and `dffy --staged` shows what's staged. Anything that names `HEAD` fails, as it does in git:

```
$ dffy show
error: unknown revision 'HEAD'
$ git log
fatal: your current branch 'main' does not have any commits yet
```

`git log`, `git show` and dffy as their pager have nothing to show until there's a commit.

## Line endings and filters

Working-tree files are compared as git would store them. Line-ending conversion (`core.autocrlf`, `core.eol`, `text` and `eol` in `.gitattributes`), `ident`, `working-tree-encoding` and clean filters (Git LFS, `filter.<name>.clean` and `.process`) apply first. A fresh checkout of a repository with `* text=auto eol=crlf` shows no changes.

- Filters run only for files whose size or timestamps differ from the index, the same files git reads.
- When a filter fails, for example because its program isn't installed, the file is shown as it is on disk with a note. Its diff can then show changes git wouldn't.
- A file whose line endings changed and nothing else is listed as modified, as git lists it, and its diff is empty.

## Symbolic links in paths

A tracked file under a directory that was replaced by a symbolic link shows as deleted, and the link as untracked, as git shows them. dffy never reads through the link, and `e` doesn't open the file.

## When dffy runs git

dffy reads the repository with its own code and writes checkpoints itself. It runs git for a few viewer actions: `git apply` to stage, unstage and discard, `git add` to mark a resolved conflict, and on a code host's review, `git rev-list` and `git merge-base` to count new commits and spot a rebase. `dffy doctor` checks that git is installed.

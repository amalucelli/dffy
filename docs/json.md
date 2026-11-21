# JSON output

`dffy --json` writes a diff as JSON for scripts, CI jobs and agents.

```sh
dffy --json main..HEAD
dffy --json --syntax -- src/
dffy --json --staged | jq -r 'select(.type == "file") | .path'
```

The output is [JSON Lines](https://jsonlines.org), one object per line, in this order:

1. a [header](#the-header): what was compared, and with which options;
2. one [file object](#files) per file, in the order printed output lists them (git's path order, renames paired);
3. an [end object](#the-end-object) with totals.

Objects stream: the first file arrives before the rest of the diff is computed. With renames on, files from the first addition or deletion onward wait for the rename pass. `--no-renames` streams them all.

This page is the schema, version 1.

## Command line

```
dffy --json [--syntax] [--highlight-spans] [--show-comments [--session]] [-w | -b] [-U<n>] [--no-renames] [-R] [<rev>...] [-- <path>...]
dffy --json --stat | --name-only | --name-status [--syntax] [options] [<rev>...] [-- <path>...]
dffy --json [options] <path> <path>
```

What to compare works as in printed output: revisions, `--staged`, `--unstaged`, `--all`, `--merge-base`, `--since-checkpoint`, paths after `--`, `--focus`, and two files or folders outside git ([usage.md](usage.md#what-to-compare)).

Options that change the objects:

| Option | Effect |
|---|---|
| `--syntax` | Turns on the syntax diff. Needed for the `moved` and `reformatted` marks, and to classify a file whose lines only moved or were reformatted. |
| `-w`, `-b` | Decide which lines differ. |
| `-U<n>` | Sets the context lines. |
| `--no-renames` | Lists a renamed file as a deletion and an addition. |
| `-R` | Swaps the sides. |
| `--highlight-spans` | Adds syntax highlighting to each line ([Highlighting](#highlighting)). |
| `--show-comments` | Adds each file's review comment threads, placed on its lines ([Annotations](#annotations)). |
| `--stat`, `--name-only`, `--name-status` | Gives a [summary](#summaries): file objects without hunks. All three give the same objects. |

- The config file's `context`, `ignore-all-space`, `ignore-space-change`, `renames` and `syntax-diff` apply as in printed output. The header records the values in effect.
- Colors, themes, paging, layout and width settings never apply. Output always goes straight to stdout.
- Exit status is 0, or 1 with `--exit-code` when any file differs. A file that `-w` or `-b` found unchanged gets an object but doesn't count as a difference. On an error, dffy writes it to stderr, exits with 128, and writes no end object.
- Not supported: a diff read from stdin (`dffy --json -`), `show` (use `dffy --json <rev>^!`), `--color-only`, `--watch`, and `--highlight-spans` or `--show-comments` with a summary.

## Conventions

- Every object has a `type`: `header`, `file` or `end`.
- Field names are `snake_case`. Values that name something (statuses, kinds, marks) are lowercase words with `_`.
- Line numbers are 1-based, as in git's hunk headers.
- Every field of the header, the file, the hunk and the end object is always present, and `null` when it doesn't apply. The exception is the file's `annotations`, present only with `--show-comments`.
- A line always has `kind`, `old`, `new` and `text`. Its other fields are left out when false or empty, which keeps context lines small.
- See [Text and encoding](#text-and-encoding) for how text is written.

## Versions

The header's `version` is `1`.

- Compatible changes keep the version: new fields in any object, new object types, and new values for `mark`, `classification`, `syntax` and the highlight roles. A consumer must ignore fields and object types it doesn't know. It treats an unknown `mark` as no mark, an unknown `classification` as `null`, and an unknown role as no highlighting.
- Breaking changes bump the version: removing or renaming a field, changing a field's type or meaning, or changing the framing. A consumer should refuse a version it doesn't know.

## The header

```json
{"type":"header","version":1,"old":{"kind":"tree","id":"4b825dc642cb6eb9a060e54bf8d69288fbee4904"},"new":{"kind":"worktree"},"untracked":true,"revisions":[],"paths":["src"],"options":{"context":3,"whitespace":"exact","syntax":false,"renames":true,"highlights":false,"summary":false}}
```

| Field | Meaning |
|---|---|
| `version` | The schema version, `1`. |
| `old`, `new` | The two sides, after revisions are resolved and `-R` is applied. See below. |
| `untracked` | Whether untracked files are listed: true for plain `dffy`, `--all` and `--since-checkpoint`. With `--since-checkpoint`, `old` is the checkpoint's tree. |
| `revisions` | The revision arguments as typed, such as `["main..HEAD"]`. Empty for none, and for two paths compared outside git. |
| `paths` | The paths the diff is limited to, relative to the repository root. Empty for none. |
| `options.context` | Context lines around each change, or `"full"` for whole files. |
| `options.whitespace` | `exact`, `ignore_all_space` (`-w`) or `ignore_space_change` (`-b`). |
| `options.syntax` | Whether the syntax diff is on. |
| `options.renames` | Whether deleted and added files are paired as renames. |
| `options.highlights` | Whether lines carry highlights (`--highlight-spans`). |
| `options.summary` | Whether this is a [summary](#summaries): file objects without hunks. |

A side is one of:

| Side | Meaning |
|---|---|
| `{"kind":"tree","id":"<hex>"}` | A tree object: a revision's tree, the merge base's for `a...b`, or the empty tree before the first commit. The id is the tree's, not a commit's. |
| `{"kind":"index"}` | The index (staging area). |
| `{"kind":"worktree"}` | The working tree. |
| `{"kind":"path","path":"<path>"}` | A file or folder compared outside git, as given on the command line. `-` is stdin. |

## Files

```json
{"type":"file","path":"src/lib.rs","old_path":"src/lib.rs","status":"modified","similarity":null,"old_mode":"100644","new_mode":"100644","binary":false,"too_large":false,"language":"rust","added":1,"removed":1,"syntax":null,"classification":null,"note":null,"hunks":[…]}
```

| Field | Meaning |
|---|---|
| `path` | The path on the new side, relative to the repository root (or the compared folder), with `/` separators. For a deletion, the deleted path. |
| `old_path` | The path on the old side. Same as `path` unless the file was renamed. |
| `status` | `added`, `deleted`, `modified`, `renamed`, `type_changed` (between regular file, symlink and submodule), `unmerged` or `untracked`. An `unmerged` file is still conflicted in the index: its new side is the working-tree file, and `new_mode` is null when that file is gone. |
| `similarity` | A rename's similarity, 0 to 100, as git computes it. `null` otherwise. |
| `old_mode`, `new_mode` | The octal mode on each side (`100644`, `100755`, `120000`, `160000`). `null` for a side that doesn't exist. |
| `binary` | Either side has a NUL byte in its first 8000 bytes. The file then has no hunks. |
| `too_large` | Either side is 64MB or more. The file then has no hunks. |
| `language` | The language dffy highlights and syntax-diffs the file in (`rust`, `c`, `python`…), from its name and first line. `null` without a bundled grammar. |
| `added`, `removed` | Lines added and removed. `null` for a binary or too-large file. |
| `syntax` | The syntax diff's outcome. `null` when it's off, or when the file has no lines to diff (binary, too large, mode change only). See below. |
| `classification` | What the change amounts to, when dffy can tell. `null` otherwise. See below. |
| `note` | Why the diff may differ from git's, or is missing: the file couldn't be read, or its clean filter failed and it was compared as read from disk. Usually `null`. |
| `annotations` | Only with `--show-comments`, which a summary doesn't take: the file's line comment threads, placed on this diff ([Annotations](#annotations)). Empty for a file without any. Left out otherwise. |
| `hunks` | The changes, in order ([Hunks](#hunks)). Empty for a binary or too-large file, a mode-only change, and a file `-w` or `-b` found unchanged. `null` in a summary. |

`syntax` values:

| Value | Meaning |
|---|---|
| `complete` | The whole file was diffed by syntax. |
| `partial` | Some changed regions were too large to match in time. They keep the line diff's emphasis and marks. |
| `unknown_language` | No bundled grammar. The file has the line diff. |
| `too_large` | A side is over 2MB. The file has the line diff. |
| `parse_timeout` | A side took too long to parse. The file has the line diff. |

`classification` values:

| Value | Meaning |
|---|---|
| `formatting_only` | Only whitespace or line breaks changed. With `-w` or `-b`: every change is whitespace, and the file has no hunks. With `--syntax`: every changed line is `reformatted`. |
| `moved_only` | With `--syntax`, every changed line is `moved`: code was rearranged, not edited. Unlike a reformat, a move can change what the code does. |

Only a `modified` file with the same mode on both sides and at least one changed line gets a classification. Renames, mode changes and type changes are changes of their own. A file that mixes `moved` and `reformatted` lines, or has any changed line without a mark, gets none. Without `--syntax`, only the whitespace case applies.

## Summaries

With `--stat`, `--name-only` or `--name-status`, the header has `options.summary: true` and every file object has `hunks: null`. All other fields are present. The end object's totals match full output.

```json
{"type":"file","path":"lib.rs","old_path":"lib.rs","status":"modified","similarity":null,"old_mode":"100644","new_mode":"100644","binary":false,"too_large":false,"language":"rust","added":1,"removed":1,"syntax":null,"classification":null,"note":null,"hunks":null}
```

| | Without `--syntax` | With `--syntax` |
|---|---|---|
| Work per file | Both sides are read and line-diffed, as for `--stat`, without building hunks or keeping the text. | The line diff and the syntax diff, as in full output. Only the hunks are left out. |
| `added`, `removed`, `binary`, `too_large` | Exact, as in full output. | Exact, as in full output. |
| `language` | From the file name only. A file known only by its `#!` line has `null`. | From the name and first line, as in full output. |
| `syntax` | `null`. | As in full output. |
| `classification` | `formatting_only` for a file `-w` or `-b` found unchanged. Never `moved_only`. | As in full output. |
| `note` | A file that couldn't be read has the error, and `null` counts. A failed clean filter isn't reported. | As in full output. |

## Hunks

```json
{"old_start":1,"old_lines":3,"new_start":1,"new_lines":3,"heading":null,"lines":[…]}
```

| Field | Meaning |
|---|---|
| `old_start`, `old_lines` | The old lines the hunk covers, as in git's `@@ -old_start,old_lines`. When `old_lines` is 0, `old_start` is the line before the insertion point. |
| `new_start`, `new_lines` | The same for the new side. |
| `heading` | The function context git prints after the second `@@`: the nearest line above the hunk that starts with a letter, `_` or `$`, without trailing whitespace, cut to 80 bytes. `null` when there's none. Display text only. |
| `lines` | The hunk's lines in order. In each run of changes, removed lines come before added ones. |

A context line's `text` is the new side's. With `whitespace: exact`, applying every file's hunks to its old side gives its new side, with or without `--syntax`. With `-w` or `-b`, a context line may differ from the old side's line in whitespace.

## Lines

```json
{"kind":"removed","mark":"modified","old":1,"new":null,"text":"pub fn add(a: i32, b: i32) -> i32 {","emphasis":[[14,17],[22,25],[30,33]]}
{"kind":"added","mark":"modified","old":null,"new":1,"text":"pub fn add(a: i64, b: i64) -> i64 {","emphasis":[[14,17],[22,25],[30,33]]}
{"kind":"context","old":2,"new":2,"text":"    a + b"}
```

| Field | Meaning |
|---|---|
| `kind` | `context` (both sides), `removed` (old side only) or `added` (new side only). |
| `mark` | What the diff found about a changed line. Left out when there's nothing to say. See below. |
| `old`, `new` | The line's number on each side. `null` on the side it isn't on. |
| `text` | The line without its `\n`. A preceding `\r` stays. |
| `no_newline` | True on the last line of a side with no final newline (git's `\ No newline at end of file`). |
| `lossy` | True when the line isn't valid UTF-8 ([Text and encoding](#text-and-encoding)). |
| `block_start` | True on a line that starts a new change block, so the removed lines before it don't line up with the added lines after it. Only the syntax diff sets it. |
| `emphasis` | The parts of a changed line that changed, as `[start, end]` byte ranges of `text`. See below. |
| `highlights` | With `--highlight-spans`, the line's syntax highlighting ([Highlighting](#highlighting)). |

`mark` values:

| Mark | Meaning |
|---|---|
| `modified` | The line is an edited version of a line on the other side. dffy paired the two, and `emphasis` shows what changed. The partner is a nearby changed line on the other side. The schema doesn't name it. |
| `moved` | With `--syntax`: the code moved without edits. The same tokens, in the same order, are among the other side's changed lines elsewhere in the file. |
| `reformatted` | With `--syntax`: the code wasn't edited. The same tokens sit in the same place on the other side, and only whitespace or line breaks changed. |

A changed line without a mark was wholly removed or added. `moved` and `reformatted` lines have no emphasis.

### Change blocks

A change block is a run of removed lines followed by a run of added lines. It ends at a context line or at a line with `block_start`. Side by side, a block's k-th removed line sits beside its k-th added line. That alignment is a layout choice: a `modified` line's partner isn't always the line beside it.

### Emphasis

- Ranges are sorted, don't overlap, and are `[start, end]` with `end` excluded.
- Offsets are bytes of `text` encoded as UTF-8, which are the line's original bytes. They aren't character or UTF-16 offsets.
- Context, `moved` and `reformatted` lines have none. Without `--syntax`, only `modified` lines have emphasis. With it, an unmarked changed line whose code was partly edited in place may have some.
- Without `--syntax`, a paired line is split into words, numbers, whitespace and punctuation, and the tokens that differ from its partner's are emphasized.
- With `--syntax`, emphasis covers tokens with no equal on the other side, and tokens whose equal is out of order (swapped arguments). Regions the syntax diff fell back on (`syntax: "partial"`) keep the line diff's emphasis.
- Lines longer than 4KB have none.

### Highlighting

With `--highlight-spans`, each line of a file in a known language has `highlights`: `[start, end, role]` triples. They're byte ranges of `text` as for emphasis, sorted and not overlapping. Text outside them has no role.

```json
{"kind":"context","old":212,"new":212,"text":"    pub color_only: bool,","highlights":[[4,7,"keyword"],[8,18,"property"],[18,19,"punctuation"],[20,24,"type"],[24,25,"punctuation"]]}
```

The roles are the names themes style, without their `syntax-` prefix ([themes.md](themes.md)): `keyword`, `string`, `escape`, `comment`, `function`, `type`, `number`, `constant`, `operator`, `punctuation`, `property`, `variable-builtin`, `attribute`, `tag`, `label`. dffy maps each grammar's capture names onto these.

- A side with no bundled grammar, over 512KB, or too slow to parse has no `highlights` on its lines.
- It's off by default. It makes the output much larger and dffy slower, since every side is parsed.
- It doesn't change `emphasis`.

## Annotations

With `--show-comments`, each file object has `annotations` before `hunks`: the review comment threads on the file's lines ([comments.md](comments.md)). Each is placed on this diff by its anchor, as the viewer places it.

```json
{"id":"9f3c2a71b04e5d18","source":"comments","side":"new","start":13,"end":15,"lost":false,"thread":[{"id":"9f3c2a71b04e5d18","path":"src/lib.rs","side":"new",…,"text":"This can overflow.",…},{"id":"1b7e94c0d25a6f03","path":"src/lib.rs","text":"Fixed, thanks.",…,"in_reply_to":"9f3c2a71b04e5d18"}]}
```

| Field | Meaning |
|---|---|
| `id` | The `id` of the thread's first comment. |
| `source` | Where the annotation comes from. `comments`, the review comments, is the only source so far. A consumer should skip a source it doesn't know. |
| `side` | `old` or `new`: the side its lines are on in this diff. With `-R`, that's the opposite of the side it was written on. |
| `start`, `end` | Its first and last lines on `side`, 1-based, as they are now. They follow the code when the file changed since the thread was written. `null` when it's lost. |
| `lost` | True when its lines weren't found in this version of the file, or were found more than once. `side` is then the side it was written on. |
| `thread` | The thread's comments as the comments file holds them ([comments.md](comments.md#the-file-version-1)), unknown fields included: the first comment, then its replies in the order they were added. Their own `start` and `end` are the lines as written, not as placed. |

- Threads come from `--comments <file>`, from the review session the viewer has open with `--session`, or else from the repository's local file. Only threads on the current branch or on no branch are included ([comments.md](comments.md#branches)). They're ordered by the first line they were written on.
- A thread belongs to the file its `path` names. A thread on a renamed file's old side goes on the rename. When the diff lists the rename as a deletion and an addition, it goes on the deletion.
- Comments on a whole file and general comments have no lines, so they aren't annotations. `dffy comments --json` has them.
- Without `--show-comments`, no object has `annotations` and no comments file is read.

## Text and encoding

The output is UTF-8. File content and paths are bytes and may not be valid UTF-8.

- A line that isn't valid UTF-8 has `lossy: true`. Its `text` has each invalid sequence replaced by U+FFFD. It has no `emphasis` and no `highlights`, since offsets wouldn't index `text`.
- Paths, headings and notes that aren't valid UTF-8 are decoded the same way, without a flag.
- A valid UTF-8 line's `text` is exactly its bytes, so `emphasis` and `highlights` offsets index `text` encoded as UTF-8.
- Control characters are escaped as JSON requires (`\t`, `\r`, `\u001b`). Nothing else is changed or escaped.

## The end object

```json
{"type":"end","files":3,"added":12,"removed":4}
```

| Field | Meaning |
|---|---|
| `files` | File objects written. |
| `added`, `removed` | Lines added and removed across every file. |

It comes last, and only when the whole diff was written. A stream without it was cut short.

## Limits

Past a limit, the output says so in a field instead of failing. The limits match the viewer and printed output.

| Limit | Value | Over it |
|---|---|---|
| Binary | A NUL byte in the first 8000 bytes of either side | `binary: true`. No hunks, `null` counts. |
| File size | Either side 64MB or more | `too_large: true`. No hunks, `null` counts. The sides aren't read. |
| Syntax diff, file size | A side over 2MB | `syntax: "too_large"`. The file has the line diff. |
| Syntax diff, parse time | 400ms per side | `syntax: "parse_timeout"`. The file has the line diff. |
| Syntax diff, matching | 10⁶ work units and 50ms per changed region, 250ms per file | `syntax: "partial"`. Unmatched regions keep the line diff's emphasis and get no marks. |
| Emphasis | Lines over 4KB | No `emphasis` on that line. |
| Highlighting | A side over 512KB, or whose parse takes over 200ms | No `highlights` on that side's lines. |

The syntax diff's time limits are wall-clock. On a loaded machine, a file can get `partial` or `parse_timeout` where it otherwise wouldn't. The line diff has no limit below the file size: a 60MB text file gets every line.

Memory stays bounded however many files the diff has. A file larger than a few megabytes of JSON is written in parts as it's encoded, not held whole.

## What's not in it

- Presentation: colors, themes, widths, wrapping, and the split or unified layout. A consumer lays out `lines` itself, pairing removed and added lines through `kind`, `mark` and `block_start`.
- Unchanged lines outside the hunks. Read the file at the side's revision.
- Commit metadata. For a commit, compare `<rev>^!`.
- Whether an uncommitted change is staged. Compare with `--staged` or `--unstaged` to see each side.

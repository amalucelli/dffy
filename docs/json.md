# JSON output

`dffy --json` writes a diff as JSON for scripts, CI jobs and agents. The output is [JSON Lines](https://jsonlines.org): one JSON object per line, in this order:

1. one header, which says what was compared and with which options;
2. one object per file, in the order printed output lists files (git's path order, renames paired);
3. one end object, with totals.

```sh
dffy --json main..HEAD
dffy --json --syntax -- src/
dffy --json --staged | jq -r 'select(.type == "file") | .path'
```

Each object is written as soon as it's ready, so the first file arrives before the rest of the diff is computed, and memory doesn't grow with the size of the diff. As in printed output, with renames on, files from the first addition or deletion on wait for the rename pass; `--no-renames` streams them all.

This page is the schema, version 1.

## Command line

```
dffy --json [--syntax] [--highlight-spans] [-w | -b] [-U<n>] [--no-renames] [-R] [<rev>...] [-- <path>...]
dffy --json --stat | --name-only | --name-status [--syntax] [options] [<rev>...] [-- <path>...]
dffy --json [options] <path> <path>
```

- **What to compare:** the same as printed output: revisions, `--staged`, `--unstaged`, `--all`, `--merge-base`, paths after `--`, `--focus`, and two files or folders outside git ([usage.md](usage.md#what-to-compare)).
- **Options that change the objects:**
  - `--syntax` turns on the syntax diff. The `moved` and `reformatted` marks, and the classification of a file whose lines only moved or were reformatted, need it.
  - `-w` and `-b` decide which lines differ.
  - `-U<n>` decides the context lines.
  - `--no-renames` lists renamed files as a deletion and an addition.
  - `-R` swaps the sides.
  - `--highlight-spans` adds syntax highlighting to each line (see [Highlighting](#highlighting)).
  - `--stat`, `--name-only` or `--name-status` gives a [summary](#summaries): file objects without hunks. All three give the same objects.
- **Settings:** the config file's `context`, `ignore-all-space`, `ignore-space-change`, `renames` and `syntax-diff` apply as for printed output. The header records the values in effect.
- **Never applies:** colors, themes, paging, layout and width settings. The output always goes straight to stdout.
- **Exit status:** 0, or with `--exit-code` 1 when any file differs. A file that `-w` or `-b` found unchanged gets an object but doesn't count as a difference. On an error, dffy writes it to stderr and exits with 128. The end object is then missing.
- **Not supported:** a diff read from stdin (`dffy --json -`), `show` (use `dffy --json <rev>^!`), `--color-only`, `--watch`, and `--highlight-spans` with a summary.

## Conventions

- Every object has a `type`: `header`, `file` or `end`.
- Field names are `snake_case`. Values that name something (statuses, kinds, marks) are lowercase words with `_`.
- Line numbers are 1-based, as in git's hunk headers.
- **Fields that are always present:** every field of the header, the file, the hunk and the end object, and a line's `kind`, `old`, `new` and `text`. They're `null` when they don't apply.
- **Fields that are left out:** a line's other fields when they're false or empty. This keeps context lines small.
- Text is described in [Text and encoding](#text-and-encoding).

## Versions

The header's `version` is `1`.

- **Compatible changes** keep the version: new fields in any object, new object types, and new values for `mark`, `classification`, `syntax` and the highlight roles. A consumer must ignore fields and object types it doesn't know, and treat an unknown `mark` as no mark, an unknown `classification` as `null`, and an unknown role as no highlighting.
- **Breaking changes** bump the version: removing or renaming a field, changing a field's type or meaning, or changing the framing. A consumer should refuse a version it doesn't know.

## The header

```json
{"type":"header","version":1,"old":{"kind":"tree","id":"4b825dc642cb6eb9a060e54bf8d69288fbee4904"},"new":{"kind":"worktree"},"untracked":true,"revisions":[],"paths":["src"],"options":{"context":3,"whitespace":"exact","syntax":false,"renames":true,"highlights":false,"summary":false}}
```

| Field | Meaning |
|---|---|
| `version` | The schema version, `1`. |
| `old`, `new` | The two sides, after revisions are resolved and `-R` is applied. See below. |
| `untracked` | Whether untracked files are listed: true for plain `dffy` and `--all`. |
| `revisions` | The revision arguments as typed, such as `["main..HEAD"]`; empty for none and for two paths compared outside git. |
| `paths` | The paths the diff is limited to, relative to the repository root; empty for none. |
| `options.context` | Context lines around each change, or `"full"` for whole files. |
| `options.whitespace` | `exact`, `ignore_all_space` (`-w`) or `ignore_space_change` (`-b`). |
| `options.syntax` | Whether the syntax diff is on. |
| `options.renames` | Whether deleted and added files are paired as renames. |
| `options.highlights` | Whether lines carry highlights (`--highlight-spans`). |
| `options.summary` | Whether this is a [summary](#summaries): file objects without hunks. |

A side is one of:

| Side | Meaning |
|---|---|
| `{"kind":"tree","id":"<hex>"}` | A tree object: a revision's tree, the merge base's for `a...b`, or the empty tree before the first commit. It's the tree's id, not a commit's. |
| `{"kind":"index"}` | The index (staging area). |
| `{"kind":"worktree"}` | The working tree. |
| `{"kind":"path","path":"<path>"}` | A file or folder compared outside git, as given on the command line; `-` is stdin. |

## Files

```json
{"type":"file","path":"src/lib.rs","old_path":"src/lib.rs","status":"modified","similarity":null,"old_mode":"100644","new_mode":"100644","binary":false,"too_large":false,"language":"rust","added":1,"removed":1,"syntax":null,"classification":null,"note":null,"hunks":[…]}
```

| Field | Meaning |
|---|---|
| `path` | The path on the new side, relative to the repository root (or to the compared folder), with `/` between directories. For a deletion, the path that was deleted. |
| `old_path` | The path on the old side: the same as `path` unless the file was renamed. |
| `status` | `added`, `deleted`, `modified`, `renamed`, `type_changed` (between regular file, symlink and submodule), `unmerged` (still conflicted in the index; the new side is the working-tree file, `new_mode` null when it's gone) or `untracked`. |
| `similarity` | A rename's similarity, 0 to 100, as git computes it; `null` otherwise. |
| `old_mode`, `new_mode` | The octal mode on each side (`100644`, `100755`, `120000`, `160000`); `null` for a side that doesn't exist. |
| `binary` | A NUL byte in the first 8000 bytes of either side. The file then has no hunks. |
| `too_large` | Either side is 64MB or more. The file then has no hunks. |
| `language` | The language dffy highlights and syntax-diffs the file in (`rust`, `c`, `python`…), from its name and first line; `null` when it has no bundled grammar. |
| `added`, `removed` | Lines added and removed; `null` for a binary or too-large file. |
| `syntax` | The syntax diff's outcome. `null` when it's off, or for a file with no lines to diff (binary, too large, only a mode change). See below. |
| `classification` | What the change amounts to, when dffy can tell; `null` otherwise. See below. |
| `note` | Why the diff may differ from git's, or is missing: the file couldn't be read, or its clean filter failed and the file is compared as read from disk. `null` normally. |
| `hunks` | The changes, in order (see [Hunks](#hunks)). Empty for a binary or too-large file, a change of mode only, and a file `-w` or `-b` found unchanged. `null` in a summary. |

The `syntax` values:

| Value | Meaning |
|---|---|
| `complete` | The whole file was diffed by syntax. |
| `partial` | Some changed regions were too large to match in time and keep the line diff's emphasis and marks. |
| `unknown_language` | No bundled grammar: the file has the line diff. |
| `too_large` | A side is over 2MB: the file has the line diff. |
| `parse_timeout` | A side took too long to parse: the file has the line diff. |

The `classification` values:

| Value | Meaning |
|---|---|
| `formatting_only` | Only whitespace or line breaks changed. With `-w` or `-b`, a file whose changes are all whitespace (it has no hunks). With `--syntax`, a file whose changed lines are all `reformatted`. |
| `moved_only` | With `--syntax`, every changed line is `moved`: code was rearranged, not edited. A move can change what the code does, where a reformat can't. |

A classification needs a file whose status is `modified`, with the same mode on both sides and at least one changed line. A rename, a mode change or a type change is a change of its own. A file whose changed lines mix `moved` and `reformatted` lines, or that has any line without a mark, has none. Without `--syntax` only the whitespace case applies.

## Summaries

With `--stat`, `--name-only` or `--name-status`, the header has `options.summary: true` and each file object has `hunks: null`. Every other field is there. The end object's totals are the same as in full output.

```json
{"type":"file","path":"lib.rs","old_path":"lib.rs","status":"modified","similarity":null,"old_mode":"100644","new_mode":"100644","binary":false,"too_large":false,"language":"rust","added":1,"removed":1,"syntax":null,"classification":null,"note":null,"hunks":null}
```

What each field costs depends on `--syntax`:

| | Without `--syntax` | With `--syntax` |
|---|---|---|
| Work per file | Both sides are read and their lines diffed, as `--stat` does, without building hunks or keeping the text. | The line diff and the syntax diff, as in full output; only the hunks are left out. |
| `added`, `removed`, `binary`, `too_large` | Exact, as in full output. | Exact, as in full output. |
| `language` | From the file name only; a file known only by its `#!` line has `null`. | From the name and first line, as in full output. |
| `syntax` | `null`. | As in full output. |
| `classification` | `formatting_only` for a file `-w` or `-b` found unchanged; never `moved_only`. | As in full output. |
| `note` | A file that couldn't be read has the error, and `null` counts. A failed clean filter isn't reported. | As in full output. |

## Hunks

```json
{"old_start":1,"old_lines":3,"new_start":1,"new_lines":3,"heading":null,"lines":[…]}
```

| Field | Meaning |
|---|---|
| `old_start`, `old_lines` | The old lines the hunk covers, as in git's `@@ -old_start,old_lines`. When `old_lines` is 0, `old_start` is the line before the insertion point. |
| `new_start`, `new_lines` | The same for the new side. |
| `heading` | The function context git prints after the second `@@`: the nearest line above the hunk that starts with a letter, `_` or `$`, without trailing whitespace and cut to 80 bytes. `null` when there's none. It's display text. |
| `lines` | The hunk's lines in order. In each run of changes the removed lines come before the added ones. |

A context line's `text` is the new side's. With `whitespace: exact`, applying every file's hunks to its old side gives its new side, with or without `--syntax`; with `-w` or `-b`, a context line may differ from the old side's line in whitespace.

## Lines

```json
{"kind":"removed","mark":"modified","old":1,"new":null,"text":"pub fn add(a: i32, b: i32) -> i32 {","emphasis":[[14,17],[22,25],[30,33]]}
{"kind":"added","mark":"modified","old":null,"new":1,"text":"pub fn add(a: i64, b: i64) -> i64 {","emphasis":[[14,17],[22,25],[30,33]]}
{"kind":"context","old":2,"new":2,"text":"    a + b"}
```

| Field | Meaning |
|---|---|
| `kind` | `context` (on both sides), `removed` (old side only) or `added` (new side only). |
| `mark` | What the diff found about a changed line; left out when there's nothing to say. See below. |
| `old`, `new` | The line's number on each side; `null` on the side it isn't on. |
| `text` | The line without its `\n`. A `\r` before it stays. |
| `no_newline` | True on the last line of a side that has no final newline (git's `\ No newline at end of file`). |
| `lossy` | True when the line isn't valid UTF-8 (see [Text and encoding](#text-and-encoding)). |
| `block_start` | True on a line that starts a new change block, so the removed lines before it don't line up with the added lines after it. Only the syntax diff sets it. |
| `emphasis` | The parts of a changed line that changed, as `[start, end]` byte ranges of `text`. See below. |
| `highlights` | With `--highlight-spans`, the line's syntax highlighting. See [Highlighting](#highlighting). |

The `mark` values:

| Mark | Meaning |
|---|---|
| `modified` | The line is an edited version of a line on the other side: dffy paired the two, and `emphasis` shows what changed. The partner is a changed line of the other side nearby; the schema doesn't name it. |
| `moved` | With `--syntax`: the line's code wasn't edited, it moved. The same tokens, in the same order, are among the other side's changed lines elsewhere in the file. |
| `reformatted` | With `--syntax`: the line's code wasn't edited. The same tokens sit on the other side in the same place, and only whitespace or line breaks changed. |

A changed line without a mark is wholly removed or added. `moved` and `reformatted` lines have no emphasis.

**Change blocks and side-by-side rows.** A change block is a run of removed lines followed by a run of added lines. It ends at a context line or at a line with `block_start`. Side by side, a block's k-th removed line sits beside its k-th added line. This alignment is a layout choice: a `modified` line's partner isn't always the line beside it.

**Emphasis:**

- The ranges are sorted, don't overlap, and are given as `[start, end]` with `end` not included.
- Offsets are bytes of `text` encoded as UTF-8, which are the line's original bytes. They aren't character or UTF-16 offsets.
- Context, `moved` and `reformatted` lines have none. Without `--syntax`, only `modified` lines have emphasis; with it, a changed line without a mark whose code was partly edited in place may have some.
- Without `--syntax`, a paired line's text is split into words, numbers, whitespace and punctuation, and the tokens that differ from its partner's are emphasized.
- With `--syntax`, emphasis covers the tokens that have no equal on the other side and the tokens whose equal is out of order (swapped arguments). Regions the syntax diff fell back on (`syntax: "partial"`) keep the line diff's emphasis.
- Lines longer than 4KB have none.

### Highlighting

With `--highlight-spans`, each line of a file in a known language has `highlights`: `[start, end, role]` triples, byte ranges of `text` as for emphasis, sorted and not overlapping. Text outside them has no role.

```json
{"kind":"context","old":212,"new":212,"text":"    pub color_only: bool,","highlights":[[4,7,"keyword"],[8,18,"property"],[18,19,"punctuation"],[20,24,"type"],[24,25,"punctuation"]]}
```

The roles are the names themes style, after their `syntax-` prefix ([themes.md](themes.md)): `keyword`, `string`, `escape`, `comment`, `function`, `type`, `number`, `constant`, `operator`, `punctuation`, `property`, `variable-builtin`, `attribute`, `tag`, `label`. dffy maps the grammars' own capture names onto these.

- A file with no bundled grammar, or a side over 512KB or that takes too long to parse, has no `highlights` on that side's lines.
- `--highlight-spans` makes the output much larger and dffy slower, since every side is parsed. It's off by default.
- It doesn't change `emphasis`.

## Text and encoding

The output is UTF-8. File content and paths are bytes, and may not be valid UTF-8:

- A line that isn't valid UTF-8 has `lossy: true`. Its `text` has each invalid sequence replaced by U+FFFD, and it has no `emphasis` and no `highlights`, since its offsets wouldn't index `text`.
- Paths, headings and notes that aren't valid UTF-8 are decoded the same way, without a flag.
- A valid UTF-8 line's `text` is exactly its bytes, so `emphasis` and `highlights` offsets index `text` encoded as UTF-8.
- Control characters are escaped as JSON requires (`\t`, `\r`, `\u001b`); nothing else is changed or escaped.

## The end object

```json
{"type":"end","files":3,"added":12,"removed":4}
```

| Field | Meaning |
|---|---|
| `files` | File objects written. |
| `added`, `removed` | Lines added and removed, over every file. |

It comes last, and only when the whole diff was written. A stream without it was cut short.

## Limits

Large files and large changes are bounded as in the viewer and printed output. Past a limit, the output says so through a field instead of failing.

| Limit | Value | Over it |
|---|---|---|
| Binary | A NUL byte in the first 8000 bytes of either side | `binary: true`; no hunks, `null` counts. |
| File size | Either side 64MB or more | `too_large: true`; no hunks, `null` counts. The sides aren't read. |
| Syntax diff, file size | A side over 2MB | `syntax: "too_large"`; the file has the line diff. |
| Syntax diff, parse time | 400ms per side | `syntax: "parse_timeout"`; the file has the line diff. |
| Syntax diff, matching | 10⁶ work units and 50ms per changed region, 250ms per file | `syntax: "partial"`; the regions not matched keep the line diff's emphasis and get no marks. |
| Emphasis | Lines over 4KB | No `emphasis` on that line. |
| Highlighting | A side over 512KB, or whose parse takes over 200ms | No `highlights` on that side's lines. |

The syntax diff's time limits are wall-clock, so on a loaded machine a file can get `partial` or `parse_timeout` where it otherwise wouldn't. The line diff has no limit short of the file size: a 60MB file of text gets every line.

Output is written per file while the files after it are diffed. The bytes held for files not yet written are capped at a few megabytes. A file larger than that cap is diffed on its own and written in parts as it's encoded, once every file before it is out. So memory stays bounded however many files the diff has, and one large file isn't held whole as JSON.

## What's not in it

- **Presentation:** colors, themes, widths, wrapping, and the split or unified arrangement. Rows are a layout of the lines; a consumer lays out `lines` itself, pairing removed and added lines through `kind`, `mark` and `block_start`.
- **Unchanged lines outside the hunks:** a consumer that wants them reads the file at the side's revision.
- **Commit metadata:** for a commit, compare `<rev>^!`.
- **Status of staging:** whether an uncommitted change is staged isn't reported. Compare with `--staged` or `--unstaged` to see each side.

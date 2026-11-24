# The viewer

The viewer is dffy's full-screen view of a diff: a file tree on the left and one file's diff on the right. `dffy` opens it whenever its output is a terminal. When the output is piped, dffy prints instead ([usage.md](usage.md#viewer-or-printed-output)).

```sh
dffy              # everything you haven't committed
dffy main..       # main against HEAD
dffy pr 123       # a code host's review, with a review plugin
```

Plain `dffy` shows everything you haven't committed: staged, unstaged and untracked. Each file's title says where it stands (`modified · partly staged`). `--staged` and `--unstaged` show one side each. [git.md](git.md#staged-unstaged-and-untracked) has the details, and [usage.md](usage.md#what-to-compare) every way to pick what to compare.

Nearly every action has a key that needs no ctrl or symbol, so the viewer works from a phone keyboard too. Mouse clicks and taps are shortcuts. `space` opens the [command panel](#panels), `?` lists every key in effect with your remaps, and [keys.md](keys.md) lists every action with its default keys and how to rebind them with `keybind` lines.

The status bar names the keys to start from in the pane that has the keys. On a narrow screen it shortens them.

| Pane | Status bar |
|---|---|
| The diff | `/ search · tab tree · space commands`, with `tab comments` while the left panel shows them |
| The tree | `/ filter · tab diff · space commands` |
| The comments panel | `/ search · c filter · tab diff · space commands` |

The first run shows a one-time tip. `tips = false` turns it off.

## Reading a diff

One frame goes around the tree and the diff. Its top and bottom edges are the only chrome, two rows in total.

The top edge names the file, as in `modified · staged · src/tui/app.rs · +3 -1`. At its right it shows where you are: the [scope line](#the-scope-line) or the hunk's heading. Short of room, that goes first, then the folder, cut from the left. The name and counts stay.

The bottom edge is the status bar. It shows what's compared (`uncommitted`, `v6.0..v6.1`, `stdin`), the position (`3/269 · 1/4 hunk`), the modes and the keys. A note such as `copied 3 lines` takes the modes' place for a moment. `status-bar = top`, or `B` in the command panel, moves the status bar to the top edge.

Under 16 rows or 100 columns there's no frame ([Phones and small screens](#phones-and-small-screens)).

These keys change what the diff shows:

| Key | Does |
|---|---|
| `s` | Cycles the layout: split, unified, auto. In file view, flips the version read |
| `W` | Wraps long lines |
| `x` | Shows 20 more lines of the highlighted `⋯ N unchanged lines` gap. In file view, opens a change in place, or closes it |
| `X` | Shows every file whole, or changes only |
| `l` | Turns [file view](#file-view) on or off |
| `d` | Turns the syntax diff on or off |
| `r` | Switches a file with a renderer plugin to its rendered view (experimental, [plugins.md](plugins.md)) |

### File view

`l` shows every file as it is in one version of the diff, whole, as plain code: the newer version first, and `s` flips to the older one. The title reads as a file's, not a diff's: `file view · src/lib.rs · working tree · 3 changes`, the version named as you gave it (`working tree`, `index`, `HEAD`, `v6.1`, a short id) and the changes counted in it, or `no changes`. The status bar says which version (`reading working tree`). File view isn't remembered: the next run shows the diff, unless `file-view = true` is set in the config.

In the [command panel](#the-command-panel) one row picks among the three: `File view ‹ off ›`, `‹ working tree · your files now ›` and `‹ HEAD · last commit ›`, stepped with `h`/`l`. Where a version is picked, it says what it is: `your files now` for the working tree, `staged` for the index, `last commit` for HEAD, and `before` or `after` for the older or newer revision of a range. Two paths compared outside git go by their names alone.

Changed lines in the version read have a heavy bar, `┃`, in the gutter, as gitsigns marks them: green for added lines, blue for modified ones, red for removed ones in the older version. Unchanged lines have none.

The other version's lines don't show. A line edited into another has no row of its own: its bar marks the edit, and a comment on the old line hangs under the new one. The lines that pair with none, as split view pairs them, stand in a rule across the pane, in their place: `── 2 lines removed · x shows them ──` in the newer version for lines removed outright, `── 3 lines added · x shows them ──` in the older. `n` and `N` stop at every change and every rule, and a comment on a hidden line hangs under its rule.

`x` opens a change in place, as Zed does: its lines show as the diff shows them, removed lines on their red background and added ones on their green, under a rule that closes it again (`── 2 lines removed · x hides them ──`). It acts on the rule or edited line last tapped, else the first on screen, and on an opened change it closes it. A tap on a rule, or on an edited line's bar, does the same. In an opened change only the lines of the version read carry the bar, the other version's numbers are dim, and lines select, copy, stage and search as in the diff. Changes stay open until `s` flips the version, file view ends, or a reload moves the hunks.

A file the version read doesn't have shows the other version, and its title says so: an added file read in HEAD is `file view · src/new.rs · working tree · not in HEAD`, a deleted one read in the working tree `file view · src/gone.rs · HEAD · deleted in working tree`. A file the diff doesn't list is titled with the version it's from: `file view · notes.md · v6.1 · not in this diff`. A renamed file, or one whose mode alone changed, shows whole too. Two files or folders compared outside git are named as you typed them: `dffy old/ new/` reads `old/` and `new/`, a long path cut from the left with `…`.

What changes in file view:

- Search goes through the version read and the changes opened. Matches on the other version's hidden lines are counted, not gone to: `match 3/12 · 2 more in HEAD (s)`.
- `A`, `U` and `D` with nothing selected take the whole hunk of the line, and say so. With lines selected they're refused unless the lines are in an opened change: `staging lines needs both versions; x opens the change in place`.
- A comment, a suggestion and a copy take the version read. `s` while selecting says `file view copies the version shown`.
- The scope line reads the version read, and a hunk's `@@` numbers don't show.
- `X` switches whole files anyway, for when file view is off, and says so.
- `d` and `r` work as elsewhere, on the version read.

A piped diff has only its hunks, so it has no file view, and `l` says why.

### The scope line

The top edge shows the definitions around the top line of code on screen, outermost first: `impl Error › fn fatal`, `class Store › add()`. It follows the top line as you scroll, not the hunk. It reads the new side, or the old one on a removed line.

Each definition shows as its keyword and name. One without a keyword, such as a method or a C function, shows as `name()`. Closures and lambdas don't count. Cut short, the outer definitions go first. At the top level there's no scope, so in a hunk its numbers show (`@@ -5,7 +5,7 @@`), unless `hunk-headers = false`.

The scope line is there for Rust, Go, Python, JavaScript, TypeScript, C, C++, Java, Kotlin and Swift, once the file's syntax highlighting is ready. Otherwise the top edge shows the hunk's heading as git finds it, the nearest unindented line above the hunk: `@@ fn render()`, or the line alone with `hunk-headers = false`. That's the case for other languages, with syntax highlighting off, in a rendered view, and until the highlighting is ready. The `@@` rows in the diff, printed output and `--json` always show that heading.

### Change marks

Changed lines get a tinted background on truecolor terminals and a colored bar elsewhere (`indicators`). Output without color always uses classic `+` and `-` signs.

The tints are green, red and, for moved code, purple, mixed into the theme's background. The default theme mixes them into the terminal's own background, which dffy asks the terminal for. `background = dark` or `light` skips the question.

Colors come from the theme, which the command panel's Theme row picks. [themes.md](themes.md) lists the built-in themes and covers writing your own.

### Modified lines

A removed line and the added line it was edited into show as one modified change, as in JetBrains IDEs. These are the pairs whose changed words are emphasized.

- Both lines of an edited pair are in the theme's blue. Only the edited words are red on the old line and green on the new. The overview column marks them in blue too.
- Lines wholly removed or added stay red and green. Moved (`»`) and reformatted (`~`) code keep their own looks.
- `modified-lines = added-removed`, or `%` in the command panel, draws edited lines red and green like the rest.
- Printed output follows the config file. `--color-only` always uses red and green.

### Where a file opens

The first time you visit a file, the view starts three rows above its first change. A whole file (`X`, `context = full`) then doesn't open at line 1 with the change far below. This holds however you got there: `f`, `n` past the last change of the file before, the tree, the picker, a tap, `--focus`, or the first file at startup.

When the first change already shows on the first screen from the top, as it usually does with changes only, the file opens at its top.

On later visits the view is where you left it. Watch mode's refreshes never move it.

`g` goes to the top. `n` into a file seen before puts its first change at the top, and `N` into any file puts its last change there.

### Hidden characters

Code and paths never reach the terminal raw. Control characters show in caret notation (`^[`) and invalid UTF-8 as `�`. Bidirectional overrides and isolates (U+202A–U+202E, U+2066–U+2069) would reorder the text around them, so they show by their code point: `<U+202E>`. A line reads in the order its bytes are in, in the viewer and in printed output.

## Navigating

| Key | Moves |
|---|---|
| `j` `k` | A line. `ctrl+d` and `ctrl+u` move half a screen. |
| `g` `G` | To the top or bottom of the file |
| `n` `N` | To the next or previous change, on into the next or previous file |
| `f` `F` | To the next or previous file |
| `m` `M` | To the next or previous comment thread ([Finding](#finding)) |
| `o` | To a file found by name ([The file picker](#the-file-picker)) |
| `K` | To where a name is defined, in a peek under the line ([Go to definition](#go-to-definition)) |
| `backspace` `\` | Back to where the last jump left, or forward again ([Jumping back](#jumping-back)) |
| `tab` | The keys, between the left panel and the diff |
| `esc` | One step back: closes a panel, ends a search or filter, cancels a selection, leaves a thread or the left panel |

`t` shows or hides the left panel ([The file tree](#the-file-tree)). `e` opens `$VISUAL` or `$EDITOR` at the line last tapped, else at the top line. `q` quits.

### Jumping back

Opening a definition from a peek or a file from the picker, the tree or the all-files tree is a jump, and so is a control API request that shows a file (`view/open`, MCP's `open_in_viewer`, `comments/add`). Each one keeps the place it leaves: the file and the line at the top of the screen. `backspace` goes back there with that line at the top again, and `\` goes forward again. Moving through the tree shows the files it passes, so a file opened from the tree goes back to where the tree was entered from, and so does leaving the tree for the diff on another file than that. `n`, `f`, `g`, threads and search are moves, not jumps.

The status bar says where each jump lands and how many places are left that way: `back to fs/namei.c:120 · 3 more`. Opening the file already shown is no jump, unless it goes to a line in it. A place whose file isn't shown any more, such as a file opened outside the diff and closed since, or whose version is another one now, is skipped and counted: `skipped 1 no longer shown`. A place is its file's path and version, so it holds when the diff's list is read again. The last 100 places are kept. A commit's page and the conversation have no path, and jumps from them aren't kept. The command panel lists `Back` and `Forward` under Go while there's somewhere to go.

A jump that lands on a line marks it: the row in the theme's `landed` style and `❯` in its gutter, on every row of a wrapped line. A definition, a use and a `view/open` line show a quarter of the screen down, with the lines before it above; back and forward show the screen as it was, marking the line that was marked when it was left, else the top line. The mark stays until the view moves: a key that scrolls or goes elsewhere, a click or the wheel, or the next jump.

### Go to definition

`K` lists the names on the line last tapped, else the top line of code, in a small list under it. Typing narrows them, the arrows move, and `enter` peeks at the one picked. A ctrl+click or an alt+click on a name peeks at it at once, and on a line of code the right-click menu has `Go to definition`. It works in the version of the diff the line is in, old or new alike, in files of these languages: Bash, C, C++, C#, Elixir, Go, Java, JavaScript (with JSX), Kotlin, Lua, PHP, Python, Ruby, Rust, Scala, Swift, TypeScript (with TSX) and Zig. TypeScript, TSX and JavaScript files see each other's definitions, and so do C and C++ files (a `.h` header is read as C).

The peek hangs under the line, as a comment thread does, and has the keys until you click the code above it; a click on the peek gives them back. Its top edge reads `kmalloc_array · 3 definitions · include/linux/slab.h:612 · v6.1`, with `changed in this diff` when this diff changes the definition's lines. On the left is the code of the line the cursor is on, highlighted, a few lines above it and the rest of the body below; on the right the name's definitions, grouped by file, each with its line, and below them the lines using it ([References](#references)). Each group shows as soon as what it holds is known and fills in as more comes, the cursor staying where it is; asked from the line defining the name, the peek opens on its uses. On a narrow screen the list goes above the code. While the code above has the keys, `j`, `k`, `enter`, `d`, `s` and `r` give them back to the peek and do their work there.

| Key in the peek | Does |
|---|---|
| `j` `k` | The next or previous definition or use |
| `enter` | Opens the definition's or the use's file at its line: its entry in the diff when the diff changes it, else the whole file, listed under not in this diff |
| `d` | Shows the definition in the diff, when the diff changes its file |
| `s` | The name's definitions in the diff's other version |
| `r` | From the definitions to the uses, or back, each group keeping its place |
| `c` | Only the uses calling the name, or every use again |
| `i` | Only the uses in files the diff changes, or those in every file again |
| `m` | The uses past the first 200 files, counted |
| `esc` `q` | Closes the peek |

Names are matched, and resolved where the asking file's imports tell. The definitions are functions, types, constants and module-level variables, and the members a reader jumps to: methods, fields and properties, enum members, and the members of interfaces, traits and protocols, each with the type it's in. A Python module's name finds its file. Definitions are listed nearest first: the same file, then files it imports, then the same folder or Rust crate, then the rest; within each, vendored and generated code last (in a `vendor/`, `third_party/` or `node_modules/` folder, a Protocol Buffers file, or one marked `Code generated`, `@generated` or `DO NOT EDIT` at its top), a member of a type the file names first, a test's after other code unless the asking file is a test's, and the asking file's own package, by the manifest nearest above it (`Cargo.toml`, `go.mod`, `package.json`, `pyproject.toml`, …), before another. What counts as an import is each language's: C and C++ `#include`s, Rust `use`, relative JavaScript and TypeScript imports, Python modules, the Go module's own packages, Java, Kotlin, Scala, C# and PHP imports by the folder their package starts in, and Ruby, Lua, Elixir, Zig and Bash files by their convention. An import is followed through a re-export (a Rust `pub use`, a JavaScript or TypeScript `export … from`, a Python package's `__init__.py`) to the file defining the name; the re-export is listed after the definitions. A name the file imports comes first as what the import names, under another name too (`import { a as b }`, `use a::b as c`, `from a import b as c`, Kotlin's `import … as`, C#'s `using X = …`) and through a renamed re-export: a JavaScript default import as the module's default export, and a module or a namespace imported as its file. A Rust `mod x;` is its file, `x.rs` or `x/mod.rs`, and a TypeScript function's overload signatures give way to its implementation. A method call lists every method of that name with the type each is on. The use itself is read too: a ctrl+click or alt+click knows the name's place on its line, and `K` takes the name's first use in code on the line, so a local variable, a parameter or the import the name comes in by is listed first, marked `local`, `parameter` or `import` after its line.

The first `K` starts the navigator helper, a process of its own ([navigate.md](navigate.md)), which builds the asked version's index of definitions on one thread at a lower priority and keeps it in the disk cache for the next run. Until it's done a lookup answers from what's indexed, the files near the asking one read on the spot, and says how far it got: `partial: 12,000 of 55,191 files indexed`. It looks again once the index is complete, and again when the diff's list is read again (in watch mode, after a refresh), with the files as they are then. Ten minutes after the last peek closed, with no `K` since, the viewer closes the helper, which gives its memory back, and the next `K` starts another that reads the index back from the disk cache. A helper that exits is started again on the next ask, at most 3 times a minute; one that can't start, or keeps exiting, is said in the peek: `definitions unavailable: …`. A name with no definition found falls back to the lines that use it as a whole word, labelled as text matches: `no definition of foo indexed yet (12,000 of 55,191 files) · searching text…`. A tags query that doesn't compile is said in the status bar. Files over 512KB, and those too slow to parse, have no definitions indexed; a lookup that finds nothing says how many there are.

#### References

The peek lists the lines using the name in its version below the definitions, with no key, as a whole-word search finds them, closest files first: the asking file, what it includes or imports, its folder or crate, then the rest. Lines stream in while the search goes, and the group's heading says how far it is (`Uses · 37 files so far · searching 12,000 of 78,019`), then how many files use the name (`Uses · 12 files`). The search stops at the first 200 files with a use (`Uses · first 200 files · m more`); `m` goes on, counting the rest. Each file lists at most 1000 lines (the last says `+N more in this file`). `c` keeps only the uses calling the name: followed by its arguments, past a Rust macro's `!` or type arguments, a definition's own name not counted, read from the line rather than the file's syntax tree. `i` keeps only the uses in files the diff changes. Each filter searches again, and the heading names those on. In files of the languages go to definition reads, a use inside a comment or a string doesn't count, nor the line inside each definition of the name that names it, once the index has its file. A comment or string over several lines is followed to where it closes, in each language's forms: block comments, strings of three quotes, JavaScript templates (their `${…}` parts are code), raw strings, heredocs, Ruby's `=begin` and Lua's long brackets; what opens one inside another, as `/*` in a string, opens nothing. A search that ran while the index was still being built says `index partial: definitions may show`. `enter` opens a use as it opens a definition.

One search runs at a time: asking for another name's uses, another version's, or closing the peek stops it. It runs after the work on screen, a little at a time, and gives way to any other work waiting. Its answer is kept for the version, the asking file, the name and the filters, so a peek at them again shows it at once, unless it ran on a partial index; the working tree's and the index's answers are dropped when the diff's list is read again, and a search of a version that moved stops and starts over.

Go to definition needs git's versions of the files and the disk cache. A piped diff, two paths compared outside git and `cache = false` say why it's off.

## Search

`/` searches where the keys are.

In the diff it searches the diff's text across files, and the comments shown in it. It's case-insensitive unless the search has a capital letter. Matches are highlighted as they're found, and typing doesn't wait for a large file to be searched. `enter` goes to the first match. Then `n` and `N` go between matches instead of between changes, and the status bar counts them (`match 3/17`). `esc` ends the search.

A match in a comment lands on the row of its box that shows it, or on a folded thread's one row. `n` and `N` go through code and comments in the order they show, the Conversation's comments included.

In the tree, `/` filters the tree fuzzily. `f`/`F` and `n`/`N` keep to the files the filter keeps, and `esc` clears it.

## Selection and copy

Drag over code to select it by characters, on one side of a split. It's copied when you let go, and `y` copies it again. A double click selects a word and a triple click the line. A drag over line numbers selects whole lines.

Shift-click stretches the selection to where you click, as a drag there would, and copies it. It works on code and in a comment. With nothing selected, it selects from the last click. Many terminals keep shift-click for their own selection and never send it.

From the keyboard, `v` starts a selection at the top line and `j`/`k` extend it. `s` picks the side of a split, and `y` copies. `Y` copies the file's path.

Copies hold the code only, unless `copy-decorations = true`. They go through the terminal (OSC 52), so they work over SSH. Under tmux that needs `set -g set-clipboard on`, which `dffy doctor` checks. [troubleshooting.md](troubleshooting.md#copying-clipboard-and-osc-52) covers terminals that block copies.

For the terminal's own selection while dffy has the mouse, hold `shift` while dragging, or press `space` then `$` to hand the mouse back.

### Copying from a comment

A drag over a comment's text selects it by characters. The selection stays within that comment wherever the pointer goes. A double click selects a word and a triple click the whole comment. It's copied when you let go, and `y` copies it again. Any other key ends it, as do a click and a change to the comments. None of this works while you're writing in the thread. A click without a drag still only focuses the thread, and a click on its top line folds it.

What's copied is the Markdown the text was drawn from, as written between the selection's ends. A selection over `two blockers` in **two blockers** copies `**two blockers**`, and part of it copies `two`. An element whose whole text the selection holds comes with its markup: bold, italic, struck text, a link, `` `code` ``, a heading (`## …`), a list item (`- [x] …`), a quote, a fenced code block, a table. A double click's word comes without it. A triple click copies the comment as written, its HTML comments too.

Text drawn other than as written copies as its source: an entity as `&amp;` rather than `&`, and an image's `[image: …]` label as the whole image.

## Comments

Review comments live in a comments file ([comments.md](comments.md)). It's `--comments <file>`, or by default `dffy/comments.json` in the git directory the repository's worktrees share. That file keeps each thread on the branch it was written on ([comments.md](comments.md#branches)). A piped diff has no comments.

The viewer shows the current branch's threads and those on no branch. A comment written in it goes on the current branch. The [comments panel](#the-comments-panel) lists the other branches' threads last.

When the comments file can't be had, the viewer opens without it and the status bar says why. This happens when moving an old worktree's file to the shared place fails.

A clean working tree prints `no changes`. With threads left open on the branch, it adds how many and how to see them: `no changes · 2 open comments · dffy comments lists them, dffy HEAD~1.. shows them`.

### Reading

A thread shows in a box right under the last line it's on, in the code's column. In a split view it takes its side's half. Threads under the same line are a blank row apart.

The box's top border carries the first comment's author (`you` for your own) and how long ago it was written. On a [review from a code host](#code-host-reviews) it also says where the comment lives:

- nothing for a comment on the host;
- `draft` for yours until it's posted;
- `private` for one kept here, such as an agent's answer, a question to an agent or a mention, followed by the key that shares it as a draft: `claude · read-only sandbox · private · A shares`.

A local review posts nothing, so none of these words show there. `applied` follows once the comment's suggestion is in the working tree ([Applying a suggestion](#applying-a-suggestion)). Each reply follows a blank row, under a line of its own that carries the same for it. Authors are bold.

The box is drawn in light lines. While the thread holds a draft and isn't resolved, its corners are round and it's in the `warn` style. While it has the focus its lines are heavy, in the `warn` style when it holds a draft.

The thread's state follows its first comment's author and age: `resolved`, and `outdated` when the host marks the thread outdated. A suggested change shows as the lines it replaces (`-`) and its own (`+`).

Comments render as Markdown, the way code hosts draw them, within the box:

- Headings are bold, and the first two levels take the keyword color.
- **Bold** and _italic_ show as such, and struck text is dimmed.
- `code` and links take the colors of a rendered Markdown file ([plugins.md](plugins.md)). Links are underlined.
- Lists show with `•` or their numbers, nested ones indented. Task lists show as `☐` and `☑`.
- Quotes sit behind a `│`. Rules show as a line, and tables in box characters.
- A line break stays one, and long lines wrap to the box.
- Fenced code is indented. It's highlighted in the language its fence names when highlighting is on. Its lines, and a table's rows, are cut with `…` when they're too wide for the box.
- HTML comments, where bots keep their state, don't show. Other HTML shows dimmed, as written.
- Links only underline. `o` opens the thread, not a link in it.

A comment on a whole file sits under the file's first row, marked `on the file`.

A comment whose lines weren't found sits under the file's last row, marked with the lines it was on: `line 145 not found`, or `old lines 140–141 not found` on the old side. The top of its box shows that code as it was, with its line numbers. The commented lines are highlighted in the file's language when highlighting is on, between up to two lines of dimmed context. A commented line's sign is its side's (`+` new, `-` old), or blank when dffy recorded it unchanged. A host's threads record one side only, so theirs always carry the side's sign. Long lines are cut with `…`, as a suggestion's are, whether or not code wraps.

Commented lines keep a mark after their line numbers, on their side of a split, in the color of their thread: yellow for a thread with a draft in it, the hint color open, dim resolved. A single line shows `◆` draft, `●` open or `○` resolved. Several lines show a rail, `┌` on the first, `│` down to `└` on the last, unbroken by removed lines between them, and overlapping threads share one. With color off the rail starts on the thread's `◆`, `●` or `○` instead of `┌`, so the shape still says which it is. The tree shows each file's comment count beside its line counts, in the color of its most pressing thread.

#### Drafts

Drafts exist only on a review from a code host. They're your comments there without a `ref`, which `P` posts. A local review posts nothing, so nothing in it is a draft. A comment by someone else without a `ref` is never posted under your name. It shows `private` and stays out of the counts. `A` on its thread makes it yours, a draft.

A draft's heading says what posting it would do, when that's news. The plan comes from the review plugin, asked for in the background a moment after the drafts change and never while drawing. The heading says:

- nothing when the draft goes on its lines;
- `posts as file comment`;
- why it can't go yet, in the plugin's words, while its lines, file or thread aren't on the host, such as `lines not in the PR head`. It's `not in the review yet` when the plugin gives no words. This clears once a head has them;
- `refused: <reason>` when the host refused it before.

A comment in your pending review on the host shows `pending`.

#### Folding

A folded thread takes one row between the box's sides, in its lines: `▸ author · age · state · first line`. The row is tinted with the `selection-inactive` background, or `selection`'s while it has the focus, so it stands out from the code. Its first line is the first line of text the comment renders to, past its HTML comments, without styles. `2 replies` sits at its right end when it has any.

Resolved threads start folded. So do outdated ones with no comment that isn't posted. The rest start whole.

`z` folds or unfolds the focused thread, or the first on screen. A tap on a thread's top line (`▾` or `▸`) does the same for that thread. `Z` folds every thread, or unfolds them all. A thread you're replying in or editing shows whole.

#### Copying a thread

A drag over a comment's text, or a double or triple click on it, copies its Markdown ([Copying from a comment](#copying-from-a-comment)). The menu on a thread copies the comment right-clicked, or the whole thread as text. The whole thread puts each comment under a line like `ana · 2026-11-17`, with its author (`you` for your own) and the day it was written, a blank line apart. Keys bound to `copy_comment` and `copy_thread` act on the focused thread, or the one selected in the comments panel. They copy its comment last clicked in the diff, else its first.

`y` copies the focused thread as a prompt for a coding agent, or the one selected in the comments panel, when no text is selected in it. `space` then `"` (`copy_prompt`) and the menu's `Copy as prompt` copy the focused thread, else the selected lines. The prompt holds where they are with the file's language and the definitions around them, the thread's comments and suggestions, the lines with those around them, and the file's changes near them. It's at most 32 KiB, laid out as in [comments.md](comments.md#as-a-prompt-dffy-comments-prompt). The status bar says what was copied (`copied a thread as a prompt`, `copied 3 lines as a prompt`), and adds `cut to fit 32KB` when something was cut. A thread on another branch is moved here first (`V`).

### Finding

`m` and `M` go to the next and previous thread, on into the next or previous file with comments, the way `n` and `N` go between changes. The thread they land on takes the focus. Its box goes a third of the way down the diff, with the code it's on above it, unless all of the box shows already. Near the top or the end of the file the diff stops there. Past the diff's last thread, `m` goes on to the next file with threads that the diff doesn't list and that's on disk ([Files outside the diff](#files-outside-the-diff)).

The status bar counts the comments file's threads: `18 threads · 2 drafts`. Drafts are counted on a review from a code host, with the branch drafts shown there.

`c` in the tree, or `!` in the command panel, keeps only the files with comments. `f`/`F` and `n`/`N` keep to them. `esc` in the tree shows every file again.

`i` lists every thread by file in the left panel ([The comments panel](#the-comments-panel)). `/` searches comments too, in every file ([Search](#search)).

### Writing

`c` opens a text box right under the selected lines, or with no selection under the line `v` would start on. In a rendered view, the comment goes on the source lines the selection was drawn from. `C` opens the same box for a comment on the whole file shown, under its first row.

`S` ("Suggest change", also `space` then `S`) opens the box on the same lines, holding them as a `suggestion` fenced block below the cursor. Edit them into what they should be and say why above. Once saved, the block's lines are the comment's `suggestion`, drawn as the change it suggests. `e` edits both the same way.

In the box every key types but `enter`, which saves, as in the Claude Code and Codex CLIs. The status bar says `enter saves · alt+enter new line · esc keeps it`.

While the box for a new thread is open, a rail in the gutter, in the box's border color, marks its lines until it's saved or left: `┌` on the first, `│` down to `└` on the last, `[` for a single line. Their gutter is tinted and their code keeps its colors. The box's heading says what it writes, where, and what `enter` does: `you · new comment · lines 319–331 · enter saves`, or for a question (`a`) `you · question for claude · lines 319–331 · enter asks`, then the other keys. A single line reads `line 42`, a comment on the file `whole file`. On a narrow pane whole parts go from the right: the keys first, then the lines, then the agent's name.

| Keys | Do |
|---|---|
| `alt+enter`, `ctrl+j` | Start a new line, on every terminal |
| `shift+enter` | Starts a new line where the terminal tells it from `enter`, one that speaks the kitty keyboard protocol |
| arrows, `home`, `end`, backspace, delete | Edit |
| `alt+backspace`, `ctrl+w` | Delete back to a word's start |
| `alt+delete`, `alt+d` | Delete to a word's end |
| `alt+left`, `alt+right` (`alt+b`, `alt+f`) | Move by word |
| `ctrl+u` | Deletes back to the line's start |
| `ctrl+a`, `ctrl+e` | Go to the row's start or end |
| `ctrl+g` | Continues in `$VISUAL` or `$EDITOR` |
| `ctrl+c` | Saves the text and quits |

A paste goes in whole, its line breaks kept. On macOS, `option` is `alt` when the terminal sends it so (Ghostty's `macos-option-as-alt`, iTerm2's "Esc+"). Ghostty sends `ctrl+u` for `cmd+backspace`, `ctrl+a` for `cmd+left` and `ctrl+e` for `cmd+right`.

`enter` saves the comment. A new one leaves the keys to the diff with `comment saved · m goes to it`, and `m` right after focuses it. A reply, an edit or a comment that asks an agent keeps its thread focused, where `esc` stops the ask. An empty box saves nothing, and a comment edited down to nothing is deleted.

`esc` keeps the text too, so leaving the box never loses it. A new comment kept this way stays focused, with `draft kept · d d deletes`. A signal to stop (`SIGTERM`, `SIGHUP`) saves the box's text, as `ctrl+c` does. When a save fails, the box stays open with the text and says why. A second `esc` discards it.

`ctrl+g` hands the text so far to the editor. If the editor fails, the box opens again with the text. When saving what the editor wrote fails, the text opens in the box again where the diff shows its place. Otherwise it stays in a file of its own, which the status bar names.

`ctrl+g` needs ctrl, so `E` ("Continue in editor", also `space` then `E`) does the same from a phone. With a thread focused it opens your draft there, or a reply to it. The draft can be one `esc` kept, or one `enter` saved and `m` went to. With no thread focused it opens a new comment on the line.

A focused thread has a heavy border. `m`, `M` or a tap on it focuses one. While it has the focus these keys act on it:

| Key | Does |
|---|---|
| `r` | Replies in a box at the end of the thread. Replying to an `outdated` thread first says once what the review plugin warns about such threads, when it has a warning. dffy-review-github's is that GitHub collapses them, so a reply there is easy to miss. |
| `e` | Edits your last draft in it, in place |
| `x` | Resolves the thread, or reopens it |
| `d` `d` | Deletes your last draft in it. When that's its first comment, the whole thread goes, and so does a resolved thread with nothing on the host. |
| `o` | Opens it on its host, when it's posted |
| `a` | Asks a coding agent about it ([Asking an agent](#asking-an-agent)) |
| `s` | Applies its suggestion to the working tree, or takes an applied one back ([Applying a suggestion](#applying-a-suggestion)) |
| `f` | Hands it to your coding agent to fix ([Fixing with an agent](#fixing-with-an-agent)) |
| `y` | Copies it as a prompt ([Copying a thread](#copying-a-thread)) |
| `A` | Makes its comments that aren't posted yours, as drafts that `P` posts. These are an agent's comments, a mention, question or follow-up of yours that asked one, and your replies in a thread kept private. On a thread with none, it says `nothing here to make yours`. |
| `V` | Moves it to the current branch, when it's on another |
| `esc` | Leaves it |

Other keys keep their meaning. `r`, `x`, `e`, `d`, `o`, `s`, `f`, `y` and `A` act on a thread only while it's focused, or selected in the comments panel. The status bar lists the keys that apply, the most used first: `r reply · a ask · s apply · x resolve · f fix · e edit · d delete`, then `o open`, `A make it yours` and `V move here`.

Posted comments, those with a `ref`, belong to the host. Only resolving changes them.

Delete resolved threads (`space` then `-`, `clear_resolved`) asks first and names what goes: `delete 4 resolved threads? y/n`. `y` deletes the resolved threads shown, replies included. Any other key keeps them. On a review from a code host, a resolved thread on the host stays, since the next update would bring it back. Only its comments that aren't posted go, your drafts and those kept `private`, and the status bar says `host threads stay; resolve hides them`. Threads on other branches and branch drafts out of view stay. `dffy comments rm --resolved` does the same from a shell ([comments.md](comments.md#reading-dffy-comments)).

### Placing and reloading

A comment sits on its lines while their text matches what it was written on. Otherwise it sits on the one place nearby where those lines and the two around them match. Files with comments are diffed in the background, so every thread finds its place or is known to have none.

When a thread is placed on lines a `⋯` separator hides, those lines show with two unchanged lines above and below. Folded and resolved threads count too. A separator stays for the lines still hidden above, and one for those below. The screen stays where it was. `x` then reveals more from the gap's edges.

In a rendered view, a thread shows under the last row drawn from its lines. Threads on rows hidden in a rendered view's gaps don't show, but the comments panel still lists them.

dffy watches the comments file and shows changes from other programs as they're written, such as a review plugin or `dffy comments apply` in another shell ([comments.md](comments.md) has how). The file's `notice`, when it has one, shows in the status bar.

A reply or an edit whose thread goes while you write it, as when an update removes it, keeps your text. The box writes a comment on the file from then on. On the Conversation, the text goes to a file the status bar names.

## Asking an agent

Experimental. The agent plugin, `dffy-agent` on `PATH` ([plugins.md](plugins.md#agent-plugins)), asks Claude Code, Codex, Pi or a command of yours about a thread. The `agent` setting picks which. The plugin runs the agent read-only, and neither it nor dffy edits a file. To have an agent change the code, apply its suggestion ([Applying a suggestion](#applying-a-suggestion)) or hand the thread to your own agent session ([Fixing with an agent](#fixing-with-an-agent)).

Without the plugin on `PATH`, `a` says so at once, before any question is written.

`a` on a focused thread, or one selected in the comments panel, opens a reply in it for your question. The status bar says what each key does: `enter asks claude · empty asks now · esc keeps it`. It says `enter asks` alone until the plugin names its agent.

- `enter` with a question saves it as your reply, kept [private](comments.md#drafts), and asks with the whole thread. [The prompt](comments.md#as-a-prompt-dffy-comments-prompt) answers its last comment, your question.
- `enter` on the empty box saves nothing and asks about the thread as it is. The prompt answers its last comment.
- `esc` keeps a question the same way without asking. On an empty box it saves and asks nothing.

Elsewhere, `a` opens the text box for your question on the selected lines, or the line `v` would start on. `enter` saves it as a new thread, kept [private](comments.md#drafts), and asks about it. `esc` keeps it without asking. The menu has `Ask agent` on a thread and on a selection, `Ask claude` once the plugin named the default.

An ask goes to the agent whose answer ends the thread, else to the default one. A question that starts with `@<name>` asks that agent instead. The plugin lists its agents once it has started, so until this viewer's first ask, it's the default one.

Each ask starts afresh from the thread, with its earlier answers and your questions in it. No session is kept between asks. Cut to fit, the prompt keeps the first comment and the latest exchange, and leaves out the replies between, oldest first ([the prompt's size](comments.md#as-a-prompt-dffy-comments-prompt)).

A question stays private because posting it without the answers it goes with would put a question to an agent out of context. On a review from a code host it shows `private` until `A` on its thread makes it a draft, with the agent's answers.

### Mentions

A comment you write in the viewer that starts with `@<name>`, as `@codex is this safe?`, asks that agent. Until the plugin has started, any name asks, and one that names no agent says so. After that, only the agents it lists do, since the other names are people's. Without the plugin, a mention says nothing. Comments that come any other way never ask: imported from a host, or added by `dffy comments apply`, the control API or MCP.

A mention stays private. On a host, `@claude` would mention whoever has that name there, or set off the team's own bot. So a comment that asks is saved [private](comments.md#drafts). On a review from a code host it shows `private` and `P` leaves it, until `A` on its thread makes it a draft. A mention no agent takes is a draft again. That's one written without the plugin, or one naming none of its agents.

### While it runs

The status bar says `asking claude… thinking…`, with what the agent is doing. On the focused thread it adds `esc stops it`, and `esc` there stops that ask and the agent with everything it started. With nothing else to back out of, the status bar says `esc stops all asks`, and `esc` stops every ask running. Quitting stops them too. One ask runs per thread at a time.

While a post runs, an answer waits to be written until the post ends, since the review plugin is writing what it posted to the comments file.

The answer comes as the agent's reply in the thread, by `claude` for example. It carries a suggestion when the agent proposes a change to the commented lines. Where the plugin says how the agent ran, that follows its name, as `codex · read-only sandbox`. On a review from a code host the answer is `private`, like anyone else's comment that isn't posted, until `A` makes it yours. A failure shows the plugin's words in the status bar, as `claude isn't logged in; run claude auth login`.

The plugin starts on the first ask, never as the viewer opens. It asks which agents are installed each time it starts. While it starts, the status bar says `starting dffy-agent…`. If it hasn't said which agents there are within 2 seconds, the ask ends saying so.

## Applying a suggestion

`s` on a focused thread, or one selected in the comments panel, puts a suggestion of the thread in place of the lines it's on, in the working tree. It takes the suggestion of the comment last clicked, when that has one. Otherwise it takes the thread's last comment with one, whoever wrote it: you, a reviewer on the host, an agent, or an MCP client. The menu has `Apply suggestion` on a thread whose last suggestion isn't applied yet, and `Revert suggestion` on one whose is. The status bar lists `s apply` or `s revert` there.

It writes the file on disk, never the index or a commit. The result is an unstaged change, which `D` discards. The new lines take the line ending of those they replace, and the rest of the file stays byte for byte. An empty suggestion deletes the lines.

It writes nothing and says why in the status bar:

- when the lines changed since the suggestion was written, or moved and are in the file more than once;
- on a thread on the old version's lines, or on a whole file;
- when the file's new side is neither the working tree nor HEAD's version, as on `dffy v1..v2`: `it works on the working tree or HEAD, and this file's new side is neither`;
- on a binary file, a symbolic link, or a file git converts with a filter.

A range ending at HEAD, such as `dffy main..HEAD`, `main...HEAD` or `dffy show HEAD`, shows HEAD's version of its files. The working tree was made from that version, so the suggestion's lines are found in the file on disk, uncommitted edits included.

On someone else's review it works only once the review is checked out, with HEAD on the review's head. Until then it says `check out PR #7 first: gh pr checkout 7`, with the command the review plugin gives, else `check out PR #7 first: HEAD isn't its head`. A review opened with `dffy pr 7` shows its head's commits, not the working tree. Once that head is HEAD, the working tree was made from them, so the suggestion's lines are found in the file on disk, as always, uncommitted edits included. The review plain `dffy` found for the branch is yours and checked out already.

The status bar says what changed, and which key resolves the thread while it's open: `applied the suggestion to src/lib.rs:12-14 · x resolves`, with the lines the suggestion put there. A deleting suggestion says `deleted src/lib.rs:12-14, as suggested`. The list is read again, so the change shows at once. The thread stays open, and resolving it is yours.

The comment is then marked [`applied`](comments.md#the-file-version-1) and keeps the lines it replaced. Its heading says `applied`. Its suggestion shows dimmed under `suggested change, applied:`, without the lines it replaced.

### Taking it back

`s` on an applied suggestion puts the lines it replaced back in place of the suggested ones, and the comment is no longer marked applied: `took the suggestion back: src/lib.rs:12-14 as before`. It's refused, as applying is, when the file no longer holds the suggested lines.

A suggestion that deleted its lines leaves nothing to find their place by, so `s` says so. `D` brings them back.

On the working tree's diff, an applied suggestion whose lines aren't where the thread is any more, as after a discard, isn't applied. `s` clears the mark and applies it again, from the lines it replaced.

Where the diff shows the working tree, the thread moves to the lines the suggestion put there, through the comments file's `set` of its lines and anchor. It stays placed rather than lost. A deleting suggestion leaves no lines to move to, and a diff of HEAD's version still has the old ones, as a review opened with `dffy pr 7` does. There the thread stays where it was. A review plugin's next update may move a thread on the host back to its lines at the review's head.

The control API's `comments/applySuggestion` ([control.md](control.md)) and MCP's `apply_suggestion` ([mcp.md](mcp.md#apply_suggestion)) apply through the viewer. They never take one back, so a client can't undo what another applied. They're refused while an ask runs, with `an agent ask is running; apply with s in the viewer`, since an agent asked to only read could reach them through dffy's own MCP server.

## Fixing with an agent

`f` on a focused thread, or one selected in the comments panel, hands the thread to your own coding agent. `space` then `&` does the same from the command panel, and so does `Fix with agent` in the menu. dffy starts the agent interactively, as you'd start it, with none of its own flags. So the agent asks before it edits, as it always does. It starts on one line, never the thread's text:

```
Address dffy review thread <id>: run dffy comments prompt <id> (or the thread_prompt MCP tool), change only what the thread asks, then reply in the thread with what you changed.
```

On a review from a code host the line says `dffy comments --session prompt`.

On selected lines, `space` then `&` or the menu's `Fix with agent` first opens the text box for what should change there. `f` moves to the next file while lines are selected. `enter` saves the note as a new thread, kept [private](comments.md#drafts) since it's an instruction to your agent, then hands it over.

The agent plugin, `dffy-agent`, lists the agents a thread can go to and how each starts ([plugins.md](plugins.md#the-agent-protocol-version-1)): Claude Code, Codex, Pi and Amp, those on `PATH`. dffy picks the one the `agent` setting names, else the first listed. A setting that names none of them is refused. A command of yours takes no thread, so it leaves the pick to the list. Without the plugin, `f` says so. While the plugin starts, the status bar says `starting dffy-agent…`. If the plugin hasn't said which agents there are within 2 seconds, the status bar says that instead and the thread goes to none. Amp takes its first message only from you, so it starts bare and the line goes on the clipboard to paste.

The agent starts at the working tree's root in a new tab of your terminal, or a window where that's all it offers, as `open_terminal` opens one. That works in tmux, zellij, WezTerm, kitty with remote control on, Rex, and Ghostty and iTerm2 on macOS. In herdr, Ghostty on Linux, Terminal.app, Alacritty and terminals dffy doesn't know, the agent runs here instead. The viewer is suspended until it exits, as for the editor, and the list is read again after.

The agent's edits are unstaged changes like yours. With `--watch` (`w`) they show as it makes them, and the status bar says so when watch is off. `A`, `U` and `D` keep or undo them per hunk. Its reply comes by `dffy comments apply` or MCP's `reply_to_thread`, under its name, not yours. Resolving the thread is yours.

On someone else's review it works only once the review is checked out, as for `s`. Asking an agent and copying a thread as a prompt still work there. The prompt says the files on disk aren't the review's version, so the agent goes by the hunks it quotes.

### Every open thread at once

`space` then `^` (`send_open_threads`) takes a [checkpoint](#checkpoints), then starts the agent as `f` does, on one line that points at all your open threads:

```
Address the open dffy review threads: run dffy comments prompt --open, change only what each thread asks, reply in each thread with what you changed (dffy comments apply --as <you>).
```

[comments.md](comments.md#as-a-prompt-dffy-comments-prompt) has the prompt. Yours are the open threads you started or replied in. The command panel's Here offers it while you have some, except with lines selected or in a conflict.

The agent reads one comments file. On a review with your branch drafts beside it, that's the file with more of yours, or with as many, the one new threads go to. The status bar says how many went, the key that shows the agent's turn, and how many of yours in the other file weren't sent: `sent 5 threads to claude · ' shows its turn`. When the agent runs in dffy's own terminal, the status bar says it exited instead.

## Staging

On uncommitted changes (plain `dffy`, `--staged`, `--unstaged`), you stage, unstage and discard lines from the viewer, as `git add -p`, `git reset -p` and `git checkout -p` do. `A` stages, `U` unstages and `D` discards. The command panel has them on the same keys, and the right-click menu on a line lists those that apply.

They act on the selection's lines: every line on the rows it spans, and in a split only those of the side it's on (`s` while selecting picks it). With nothing selected they act on the hunk of the line last tapped, else the first hunk on screen.

`D` asks first: `discard 3 lines at lines 12-14 of src/app.rs? y/n`. `y` discards the lines' unstaged changes, and any other key keeps them. The question counts only the lines that would go, those that differ from the index, and says where they are in the working-tree file. Discarding an untracked file deletes it, after asking `delete the untracked file …? y/n`. The question comes a moment after `D`. A key, click or paste in between drops it (`discard dropped`), so a `y` meant for something else never answers it.

The file's staging decides which apply. Stage and discard act on unstaged changes and unstage on staged ones, so a partly staged file takes all three. `--staged` only unstages, and `--unstaged` doesn't unstage.

Picked lines follow `git add -p`'s edits. When staging, a removed line left out stays and an added line left out isn't added. Unstaging and discarding take the change back out, so there an added line left out stays and a removed line left out isn't put back.

Plain `dffy` shows `HEAD` against the working tree, so a staged hunk stays on screen and its file says `partly staged`. `A` on it again says there's nothing to stage. `U` takes it back out of the index.

Some changes can't be staged by line: renamed, binary and conflicted files, a file whose mode or type changed, symbolic links, submodules, and a file shown as on disk because its filter failed. Neither can a file's last line be split from its missing newline at the end. `git add -p` or `git add` take those.

One runs at a time. While one runs, another says `still applying; wait`, and a selection stays for when it's done.

The status bar then says what was done (`staged 3 lines`), and the diff is read again, watch mode or not, keeping your place. `git apply` writes the change, so git must be on `PATH`. It refuses when the file changed since it was shown, and the status bar says why.

A discard never takes working-tree lines edited after the diff was shown. It's refused with `the file changed since it was shown; look again`, and the edit stays. An edit elsewhere in the file doesn't stand in the way.

## Merge conflicts

A file a merge, rebase or cherry-pick stopped on (`U`, `conflicted`) shows as its working-tree file against `HEAD`, with the conflict markers highlighted. You resolve each conflict, from `<<<<<<<` to `>>>>>>>`, in the viewer. [git.md](git.md#merge-conflicts) has how the conflicts are read and the file written.

While the file at the cursor has conflicts to resolve, `n`/`N`, `o`, `t`, `b`, `a` and `w` mean what they do here. They come ahead of their meaning elsewhere and after a focused thread's. A search going on keeps `n`/`N` for its matches. Every other key does what it does anywhere, including `f`/`F` to leave the file. These keys are the `conflict:` table of [keys.md](keys.md#in-a-conflicted-file). Arriving in such a file says them once: `conflict 1/5 · o ours · t theirs · b both · w writes`, with the conflict on screen and how many there are. The status bar keeps `conflicts` while the cursor is in the file.

`n` and `N` go to the next and previous conflict of the file, with `conflict 2/5 · o keeps current · t takes incoming · b both` in the status bar. Past the last one that way, they go on to the next or previous change as elsewhere.

A choice applies to the conflict of the line last tapped. Without one, it applies to the conflict `n`/`N` went to while that's on screen, else the first on screen.

| Key | Choice |
|---|---|
| `o` | Keeps the current side (ours, what `HEAD` has) |
| `t` | Takes the incoming side (theirs) |
| `b` | Keeps both, the current side first |
| `a` | Takes the base, the lines both sides started from, when git wrote them (`merge.conflictStyle` `diff3` or `zdiff3`) |

The same key again takes the choice back. The status bar says what was chosen and how many are left: `conflict 2/5: incoming taken · 3 left · w writes`. The right-click menu on a conflict's line has the same choices.

Once a conflict has a choice, the file shows as what writing it would do: the diff from the working-tree file to the resolved one. A chosen conflict's lines, markers and all, show as removed, and the lines it keeps show as added. A conflict without a choice shows unchanged, markers highlighted. The file's counts stay those of its diff against `HEAD`. Taking the last choice back shows that diff again. Choices live until written or until you quit. When the file changes on disk so its conflicts no longer match, they're dropped and the status bar says so.

`w` writes the file as the choices resolve it. With conflicts left without a choice it asks first, `write with 2 of 5 conflicts unchosen, markers kept? y/n`, and those keep their markers. Once every conflict is resolved, it offers `wrote src/app.rs; git add it to mark it resolved? y/n`. `y` runs `git add`, and any other key leaves the file unstaged.

Some files can't be resolved by conflict: a binary file, a symbolic link or submodule, a file whose filter failed, and a file one side deleted and the other changed. For the last, the viewer says which side deleted it: `the incoming side (theirs) deleted this file and the current side changed it: git add keeps it, git rm deletes it`. The keys say why. git resolves those (`git checkout --ours`/`--theirs`, `git add`, `git rm`).

## Panels

### The quick popup

`space` opens a small popup first. It sits at the bottom right, just above the screen's last row, so the diff stays in sight. On a narrow screen it's full width at the bottom.

It lists one row per line, in three titled groups:

1. Here's rows, under what they act on: `On this thread`, `On these lines`, `In this conflict`, `On this pull request` in the review plugin's word, else `Here`.
2. View: file view first (`File view ‹ off ›`, stepping to each version as the full panel's row does), then the layout, wrapping, syntax diff and whole files.
3. Go: finding a file, the comments panel, watching, since checkpoint once there's one, and opening a terminal.

View and Go own their rows, so Here's group leaves out what they list. They in turn leave out a row whose key a Here row takes. A screen too short for all three leaves out View.

Each row shows one key. It's the key that does the same outside the panel when there is one, such as `f` for fix with agent on a thread. Otherwise it's the panel's, such as `&` with lines selected, or `_` for file view, whose `l` steps values in the panel. The popup has the full panel's keys on Here. A row's key runs it, whether shown or the panel's, and any other entry's key runs that entry. `h`/`l` on a value and a tap work the same.

The popup opens with no row highlighted, and `space` shows the full panel on Here (`space more`). A row run by its key, moved to with `j`/`k`, or tapped is highlighted; then `space` and `enter` run it again, a value stepping forward, and the popup stays open on the new state (`space toggles`). `tab` shows the full panel at any time. `/` starts its search, `?` lists all keys, and `esc` closes. A setting's key the popup doesn't list shows the full panel on that setting's tab.

### The command panel

The full panel holds every setting with its state and key, and the actions, in tabs: Here, View, Files, Review, Git and Copy. Toggles without a key of their own live here: indicators, highlighting, whitespace, line numbers and theme.

Here, where the full panel opens, has the few actions that apply to what you're on:

- On a focused thread: ask an agent, fix with an agent, apply or revert its suggestion, copy it as a prompt, resolve or reopen it, and reply.
- With lines selected: comment, suggest a change, ask, fix, copy them, or copy them as a prompt. On uncommitted changes it adds stage, unstage and discard.
- In a conflict on screen: its choices, and once one is made, writing the file.
- On a review: posting it, pulling its updates and showing what changed since your last review. On the review plain `dffy` found for the branch, it adds the whole review.
- Otherwise: sending your open threads to your agent while you have some, then showing what changed since the latest [checkpoint](#checkpoints) and taking another once one exists, then finding a file, the layout, wrapping, watching and opening a terminal.

On a thread and on a review, sending your open threads follows while you have some ([Fixing with an agent](#fixing-with-an-agent)). All keys (`?`) is last.

The other tabs list the panel's entries by kind, in the same groups as [keys.md](keys.md). A tab of several kinds titles each. View holds the layout, the diff and the display. Review holds comments, agents and the review. Git holds staging, watching and checkpoints. Copy holds copying and handing the mouse back. Entries that can't apply aren't listed: staging off uncommitted changes, the review's entries off a review, opening a review without a review plugin, and since checkpoint before there's one.

The full panel looks as `?` does. It's the same box, sized and placed on the screen as `?`'s whatever the tab holds, with the same tab strip and two footer rows. Each row shows its keys first, in the keys' color, then its label: `o   Find file…`, `W   [x] Wrap long lines`, `s   Layout ‹ unified ›`. The key that does the same outside the panel follows the panel's, dimmed. On Here a row shows one key, as the quick popup's rows do. `tab` and `shift+tab` switch tabs, as does a tap on a title. A tab takes one column while its rows fit the box's height, else two where the box is as wide as `?`'s two. On a narrow screen the panel takes the whole screen in one column.

A row's key runs it at once, from any tab, whether its row shows or not. When it can't run, it says why. A setting's key shows its tab with the cursor on it. `j`/`k` and `enter` run the row under the cursor. `/` searches every tab, each match with its tab's name. Searching the diff is its own `/`.

A key the diff binds too does the same thing here: `W` wraps in both, and `f` goes to the next file. The rest take keys the diff leaves free, a symbol where no letter is free: `#` line numbers, `@` hunk headers, `*` highlighting. On Here, the key a row shows from outside the panel runs it too, ahead of another tab's entry on the same letter. On a thread `r` replies and `x` resolves, where the View tab's `r` is the rendered view and its `x` more context.

Changing a setting keeps the panel open, so you can change several in a row. Actions that move somewhere else close it.

A setting with values shows as `Layout ‹ auto ›`. On its row, `h`/`l` or `←`/`→` step back and forward, as does a tap on `‹` or `›`. `enter` and the row's key step forward. On other rows `h`/`l` do nothing. Highlighting and line numbers are on `*` and `#`.

`enter` or a tap on the Theme row opens a list of every theme, dark and light grouped, with the current one marked `*`. Typing filters it (`cat` finds catppuccin). The theme under the cursor shows on the screen behind as you move. `enter` keeps it, and `esc` goes back to the theme you started from. `←`/`→` on the row, and its key `~`, step through themes without the list.

### The key list

`?` shows the keys in effect, one short label each, in seven tabs: Move, Files, View, Git, Copy, Review and General. A tab of several kinds titles each, as in [keys.md](keys.md). The tree's keys are a group of the Files tab. A thread's keys are a group of the Review tab, "on a thread", and a conflict's are a group of the Git tab, "in a conflict". `tab`, `←`/`→` or `1`–`7` switch tabs, and `/` searches all of them. Settings without a key are in the command panel. [keys.md](keys.md) has the full list.

### The comments panel

`i` turns the left panel into a list of every thread by file and gives it the keys. The list includes threads on files not in this diff. `i` again shows the files. It opens the panel when it's hidden. The panel's title says what it shows: `Comments · 12 threads · 1 open`.

Each file has a header: its folder dimmed, its name bold, and how many threads it has, dimmed, at the right end. A row per thread follows, with:

- the gutter's mark: `◆` your draft, `●` open, `○` resolved;
- the line (`:183`) in the `line-number` style;
- the author, bold, before the text, on a file's first row and where it changes;
- the first line of text its first comment renders to;
- its replies at the right end (`↩3`), or nothing without any.

One word shows when it matters. `moved`, in the `warn` style dimmed, marks a thread whose lines weren't found. `branch draft` marks a draft of the local review's file. `private` marks a comment kept here, as its heading says. A resolved thread's row is dimmed except for its line, its `moved` and its author's bold. A narrow panel leaves out the line, then the replies.

Files come in the tree's order, then those not in this diff, by path. A header of a file not in this diff says `enter opens the file`. It says `file not on disk` when the working tree doesn't have the file as the panel opens. It says `not in this diff` when there's no working tree to open it from, as for paths compared outside git. Within a file, threads with a draft in them come first, then open, outdated and resolved ones.

The other branches' threads come last, folded to one row. `b` shows or folds them. `V` on one moves its thread to the current branch, after which it shows on the diff. The other thread keys act on them too, writing in `$EDITOR` since the diff doesn't show them.

`j`/`k`, the arrows, the page keys, `g`/`G`, the wheel and a tap go from thread to thread, over the headers. The diff follows: it opens the thread's file and shows the thread there, focused, as `m` does. The keys stay in the panel. `m`/`M` or a tap on a thread in the diff move the selection too. A thread on a file not in this diff leaves nothing focused in the diff.

The thread in the diff is a preview. A folded one shows whole while it's selected. It folds again once the selection moves on, `m`/`M` or a tap focus another thread, or the panel closes. `z` on it keeps what it sets, as on any thread, and so does replying in it.

`r`, `e`, `x`, `d` (twice) and `o` act on the selected thread as on a focused one, whether or not the diff shows it. So do `a`, `s`, `f`, `y` and `A`. The status bar lists those that apply. On a thread the diff shows, `r` and `e` open the text box there, and the diff takes the keys. On one it doesn't show, such as a thread on a file not in this diff, they write in `$VISUAL` or `$EDITOR`. While a thread is selected these letters act on it rather than as they do in the diff.

`enter` or `tab` gives the diff the keys, on the focused thread, where the same keys act on it. `enter` on a thread on a file not in this diff opens the file first. `tab` comes back to the comments. `esc` in the panel gives the diff the keys too, as it does from the tree.

`c`, the tree's key for files with comments, steps through filters: All, Open (every thread not resolved, drafts included), Drafts (threads with a draft in them) and Resolved. `C` steps back. With a filter, the title counts what it keeps and names it: `Comments · 3/12 · open`.

`/` keeps the threads whose path (fuzzily), author or text has what's typed, within the filter. `esc` clears it, then gives the diff the keys.

### The file tree

`t` shows or hides the left panel. It shows the file tree unless it shows the comments (`i`). Folders collapse, and single-child folder chains show as one row. Each file has its status letter and line counts.

Untracked files sit in their folders among the changed ones, in path order, with a dim `?`. Once a file is [opened outside the diff](#files-outside-the-diff), the diff's files go under a heading, `Changed in this diff (N)`, and the opened ones under `Not in this diff (N)` after them. Each heading counts its files and folds. The diff, `f`/`F` and the picker go through them in that order. A commit's files go under the commit instead.

In the tree, `enter` or `l` opens, `h` collapses, and `u` goes to the parent folder. `v` marks a file viewed, which dims it, `x` closes a file opened outside the diff, and `+`/`-` resize the tree. The tree follows the diff as you move between files.

### All files

`a` in the tree, or the command panel's All files row, lists every file of the repository instead, as Zed's project panel does: one tree, the title naming the version (`All files · working tree`, `All files · v6.1`). From the diff it shows the tree and gives it the keys. It opens on the file shown, its folders opened to it, once: after that the tree stays where you leave it as you move through the diff. `a` again goes back to the diff's files, and the choice is remembered (`all-files`); coming back to all files finds its folders and place as they were.

- Folders start folded and open as you go, each made from the version's list of files, read once the tree shows and kept. Until the list comes, the diff's files alone show, and the title says `listing…`. Folded and opened folders stay so across rebuilds.
- The diff's files keep their status letter and counts, their names in their change's color, the files the version lacks included: a deleted one in the newer version, an added or untracked one in the older. A renamed file is at its path in each version. A folder with changes inside takes the color of the strongest of them: a conflict, then a deletion, an edit, an addition, an untracked file. Files without changes are plain.
- `enter` or a tap on a changed file goes to its diff. On another it shows the file whole, as the [picker](#opening-files) does, and `x` closes it again.
- `s` in the tree shows the repository at the other version of the diff.
- `/` filters every file, the diff's included, scored as the picker scores them: in the background on a large repository, the matches shown staying until the new ones come. The best 1,000 show, with their folders, and the title counts them all.

A piped diff and two paths compared have only their files, so `a` says why and the panel leaves the row out.

### The file picker

`o` opens the picker. Type part of a path, fuzzily. Matches in file names rank before matches in folder names. `enter` or a tap opens the file. The picker reads a piped diff to the end while it's open, so it finds files not yet reached.

The diff's files come first. Under a divider, `not in the diff · working tree`, come the repository's other files in the diff's newer version, and `tab` switches them to the older one (`tab HEAD (last commit)`) and back ([Opening files](#opening-files)).

### The menu

A right click opens a menu of what the keys do to what was clicked, each entry with its key, so the menu teaches them. Nothing is only in the menu. An action with no key of its own there shows the command panel's, as `␣ "` for `space` then `"`. Entries that wouldn't apply aren't listed.

A right click aims first, as a click does, but without opening, folding or revealing anything. It selects a file in the tree or a thread in the comments panel, focuses a thread, or marks the line `v` and `e` start from. A click inside the selection keeps it.

| On | Entries |
|---|---|
| A file in the tree | Copy its path (`Y`) or its absolute path (`copy_absolute_path`, no key by default). Open it in the editor (`e`). Open a terminal at the working tree's root ([`open_terminal`](#opening-a-terminal)). Mark it viewed or not (`v`). Comment on the whole file (`C`). Show only files with comments, or every file (`c`). |
| A line of code | Comment on it, or on the selection (`c`). Copy it (`v` then `y`) or the selection (`y`), or the selection as a prompt for a coding agent (`copy_prompt`). On uncommitted changes, stage, unstage or discard its hunk or the selection (`A`, `U`, `D`), as far as the file's staging leaves them to do. In a conflict, its choices (`o`, `t`, `b`, `a`), the one made reading `Undo: …`, and once a choice is made, writing the file (`w`). Open the editor at it (`e`). Open a terminal at the working tree's root (`open_terminal`). |
| A `⋯` separator | Reveal more of the lines it hides (`x`), on that separator rather than the highlighted one. |
| A thread, in the diff or the comments panel | Reply (`r`). Ask an agent about it (`a`, `Ask agent`, or `Ask claude` once the plugin named the default). Apply its suggestion (`s`), or take an applied one back (`Revert suggestion`). Fix it with your agent (`f`). Edit or delete your last draft in it (`e`, `d`, which asks for a second `d`). Make its comments that aren't posted yours (`A`, `Make it yours`), when it has some. Move it to the current branch (`V`), when it's on another. Resolve or reopen it (`x`). Fold or unfold it (`z`), when the diff shows it. Open it on its host (`o`), when it has a link. Copy the comment right-clicked as written (`copy_comment`), the whole thread with each comment under a line of its author and date (`copy_thread`), or the thread as a prompt for a coding agent (`y`). `copy_comment` and `copy_thread` have no key by default. |

`j`/`k` or the arrows move, `enter` or a tap runs an entry, and `esc` or a tap outside closes the menu. A right click elsewhere opens the menu there instead.

## Files outside the diff

### Opening files

The [picker](#the-file-picker) opens any file in either version of the diff, not only the ones it changes. Below the diff's files it lists the others in the newer version, or after `tab` in the older one: for `v6.0..v6.1` the files of `v6.0` or `v6.1`, for plain `dffy` HEAD's or the working tree's tracked files. The list is read the first time `o` opens and kept. Each keystroke scores the diff's files at once and the others in the background, so typing doesn't wait.

`enter` on one of them shows it whole after the diff's files, as it is in that version. The tree lists it under `Not in this diff`, tagged with the version's name (`v6.0`) when it's from the older one, and the title says `not in this diff`. A file the diff does list goes to its own entry. Opening an open file in the other version replaces it. A binary or very large file shows why it isn't shown, as in the diff.

When the diff's sides move, as watch mode reloads, a checkpoint or a review's update, each opened file is looked up again: it shows as its version has it now, or it closes, and the status bar says so (`closed, gone: kept.txt (v6.0)`). Tree `x`, or `space` then `)`, closes one. A file with comments stays, and `x` says so.

A piped diff and two files compared outside git list no other files, and the picker says why.

`dffy ctl open`, and the MCP tool `open_in_viewer`, open a file the same way, at the side asked for ([control.md](control.md#dffy-ctl)).

`e` opens the working tree's copy of a file. When the view shows another version of it, as a range between commits, a review or file view on the old side does, the status bar names that version: `editing the working tree · view: v6.1`. On a row of the old side alone in one of the diff's files, the editor opens at the line the change maps it to on the new side. When the view shows the file from the working tree, on either side, the file is read again once the editor exits, watch mode on or not.

### Files with comments

The comments panel lists threads on files this diff doesn't change. `enter` or a tap on such a thread opens its file after the diff's files, whole, as the working tree has it, with every line unchanged. `m` past the diff's last thread opens the next such file with threads that's on disk, the same way.

The tree lists the file under `Not in this diff`, and the title says `not in this diff`. The thread is focused on its lines. A thread whose lines are gone shows the lines it was written on at the file's end, as in any file. Replying, asking, resolving, fixing and applying a suggestion work there as on any file in the working tree.

The file stays for the session, or until the diff lists it. A suggestion applied there changes it, so in a diff of the working tree it then becomes one of the diff's files. A file that isn't on disk opens nothing. The status bar says so, and its thread stays in the panel.

## Checkpoints

A checkpoint keeps the working tree as plain `dffy` shows it. The diff can then show only what changed after it, such as what a coding agent did in its turn after you reviewed the rest. [git.md](git.md#checkpoints) has what a checkpoint holds and where it's kept.

Fix with agent (`f` on a thread) and sending your open threads (`space` then `^`) take one before they start the agent. `space` then `;` (`take_checkpoint`) takes one by hand, and the status bar says `checkpoint 3 taken · ' shows what changes since`. `dffy checkpoint` takes one from a shell. Taking one runs off the screen's thread, and a failure shows in the status bar. A Fix with agent goes on without one.

`'` shows the latest checkpoint against the working tree, with `since checkpoint · 3m ago` in the status bar. `'` again shows the diff from before. A viewer started with `dffy --since-checkpoint` shows the same, with the checkpoint's age in the status bar, and there `'` goes to plain `dffy`'s diff. A checkpoint taken while it shows becomes the one it's since. Threads stay on the lines they're anchored to. Lines can't be staged here, since a hunk since a checkpoint isn't the index's.
## Code host reviews

Experimental. A review plugin brings a code host's reviews into the viewer: a pull request, a merge request, a change. dffy uses the only one installed, unless `review = <name>` or `--review <name>` names one or `review = none` turns them off. [plugins.md](plugins.md#review-plugins) has `dffy-review-github`. The plugin runs in the background and has no screen. Everything happens in the viewer.

The command and the words are the plugin's. It names its command, what it reviews, how a review reads, and the events a review can be posted with. They show in the command panel, the picker, the status bar and the post dialog. The examples here are dffy-review-github's: `dffy pr`, "pull request", `PR #6064`. A GitLab plugin could make them `dffy mr`, "merge request", `MR !6064`. Without a description, a review is `review 6064`. Without a review plugin that describes itself there's no such command, and plain `dffy` doesn't ask for the branch's review.

### Opening a review

`dffy pr 6064` says `fetching PR #6064…` until the review's commits are in the clone. Then it shows `<base>...<head>` with the review's comments file. A review opened before shows at once. Its threads keep arriving while you read, with `fetching threads 100/312` in the status bar.

`dffy pr` opens the current branch's review. With none, it opens the usual diff with the picker open.

Plain `dffy` on a branch, with no revisions or paths, opens as usual and asks the plugin in the background for the branch's review. When there is one, what's shown stays plain `dffy`'s: your changes not committed. The review adds its comments file, its part of the status bar, `p`, `P`, and the checks for new commits. Threads on the files shown are placed by their anchors, and the rest are in the comments panel (`i`). A clean working tree still prints `no changes` without waiting for the answer.

Show whole pull request (`space` then `Q`) switches to the review's base against the working tree (`--merge-base <base>`), titled `PR #6064 + working tree`. That's the review's changes with your fixes not pushed yet. The entry is named in the plugin's words, or "Show whole review" without them. The same entry, then "Show working tree changes", goes back. `dffy pr` always shows the whole review.

Drafts written there are posted on the lines they have at the review's head. One the plugin can't post yet, such as one on lines the head doesn't have, stays a draft and says why in its heading. The words are the plugin's, or `not in the review yet` when it gives none.

The picker (`O`, or `space` then `O`) is "Open pull request…" in the plugin's words, or "Open review…" without them. It lists the reviews the plugin offers, by group. Those on this branch, those waiting on your review and yours come first, then the plugin's own groups by its titles. Each review shows how many drafts of yours it holds. `j`/`k` move, `enter` opens one in place of what's shown, and `esc` closes.

Opening or finding a review writes the session pointer, which `dffy comments --session` and agents read ([comments.md](comments.md#the-session-pointer)). It stays when you quit, until another review replaces it.

### Branch drafts

On a review, your drafts in plain `dffy`'s comments file on the current branch show too, where they place on the review's diff. That file is `--comments`, or `dffy/comments.json` in the shared git directory. They're marked `branch draft`, with what posting would do with them. Those that don't place stay out of view, out of the comments panel and out of the counts, since they're the local review's.

Editing, replying, resolving or deleting in a branch draft's thread writes the local file. A new comment goes to the review's file. On the review plain `dffy` found, it goes to the local file instead, on the branch, since it's the working tree's until it's posted. Both files are watched.

### Keeping up

The status bar starts with the review: `PR #6064 · 18 threads · 2 drafts`. It adds `closed` or `merged` when the host says so, and what the plugin is doing while it runs. Comments waiting in your pending review on the host add `2 pending · P submits`.

A plugin that watches tells dffy when the review's head moved or new comments came, on its own cadence ([review-github.md](review-github.md#watching) has dffy-review-github's). dffy never asks on a timer. When the plugin has news, the status bar says `PR #6064 updated · new commits · p to update`, or `PR #6064 · new comments · p to update`. After 10 minutes without a key, dffy tells the plugin you're idle, and it stops asking until the next key. Why asking failed is said once, until it fails another way.

`p` fetches the head and the threads and shows them in place. You stay where you are, threads move with their lines, and your drafts stay. Once the threads are in, the note counts the new commits and the new comments, or says it's up to date. New comments read `3 new comments · m goes to them`, and `m` goes to each of them in turn first. A review reopened from before counts the comments its threads bring the same way. A first open doesn't.

Since last review (`R`, or `space` then `R`) shows only what changed from the head your last review was on to the head. Pressed again, it shows the whole review. That head is the review plugin's to say. dffy-review-github keeps the head of your last review posted from dffy. Until there's one, it takes the head of your last review on GitHub. With neither, there's nothing to show since. When the review was rebased since, the range still runs from that head, so it takes in what the base gained too. The note says so: `rebased since your last review; showing 9999aaa..bbbb222, the base's changes since included`.

### Posting

`P` opens a dialog, like a code host's "Finish your review". It holds:

- What goes: your drafts as comments, replies included, those on the whole file, the threads resolved or reopened and not sent, and what's pending on the host, which an event that submits sends with them. It reads like `12 comments · 1 as a file comment · 1 resolve change · 2 pending on PR #6064`. Comments by others without a `ref` don't go. The counts are the plugin's plan, asked for in the background without reaching the host. Until the plugin answers, the dialog says `checking what can be posted…`, and `couldn't check what can be posted: …` when that failed.
- One warning, only when something won't go up: `2 drafts left out · 1 refused before · v goes to them`. `v` closes the dialog on the first of them, its thread focused, with why in the status bar. `m`/`M` go on through the threads from there. Sending anyway sends the review without them, and they stay drafts.
- A warning when the review moved since what's shown, since the review goes on the head shown. Another when the host says it's closed or merged.
- With branch drafts on the diff, `l  [x] with the 3 branch drafts on this diff`. `l` sends them with the review or keeps them on the branch. It's on when they were all written since the review opened, else off.
- The plugin's events, each with its key. The cursor starts on the first that submits, else on the one the last post sent. dffy-review-github's are `p` pending (kept as your draft on GitHub), `c` comment, `a` approve and `r` request changes. A plugin that names none has one, `p` post. `j`/`k` move too. An event that submits goes with no new drafts as well, submitting what's pending.
- The review's labels, when it's voted on beyond an event, such as a Gerrit change's `Code-Review` and `Verified`. Each is a row under the events: `Code-Review  ‹+1›  Looks good to me`. Each starts on your vote now, as the review's session says. `j`/`k` go on from the events to them. On a label, `←`/`→` (or `-`/`+`) step its vote through the values offered. Only the labels whose vote changed are sent.
- A summary. `s` (or `tab`) writes in it, and `esc` comes back. It's kept in the review's comments file as the dialog closes, comes back with it, and goes once a review that submits is sent.

`enter` first moves the branch drafts, when they go, into the review's comments file. They get new ids, replies go with their threads, and they leave the local file once the review's file has them. When that fails, nothing is sent, the dialog stays open, and both files are as they were. While a `p` update or an opening fetch still runs, `enter` sends nothing and says `still updating; post once it's done`, and the dialog stays open.

Then dffy sends the review in the background, with `posting 3/12` in the status bar. Once posted, the comments turn to the posted look, and `o` opens the review until another key is pressed. While it posts, the comments don't change. A draft saved, resolved or deleted then says `posting; edit once it's done`. A comment being written stays in its box, saved by the next `esc` once the post is done.

Drafts the host refuses don't hold the review back. It's sent without them, and each keeps the host's reason in its heading. The status bar keeps saying how the post ended: `review sent`, `review sent · 2 comments refused, kept as drafts · 1 draft left out`, the event's own words for one that leaves the review open (`review pending`), or `nothing posted`. The plugin's notice, when its post leaves one, shows over it.

### Quitting a review

Quitting stops the plugin and anything it started. While a post runs, the first `q` (or `ctrl+c`) says `still posting 37/200 · q again stops it`, since stopping halfway leaves the review half sent until the next post. The second asks the plugin to stop the post and waits up to 30 seconds for it to end, so what it posted keeps its refs and isn't posted again. A signal to stop, or the terminal closing, does the same. Leaving a review with drafts not posted says so once the screen is back: `PR #6064: 3 drafts not posted`.

### Conversation

A review's [general comments](comments.md#general-comments), those on the review rather than a file, show on its Conversation. dffy-review-github brings a pull request's description, the comments in its conversation, and the summaries submitted with reviews.

The Conversation is first in the tree, above the files, as `≡ Conversation` with how many comments it has. It's first in the comments panel too, as a group of its own. It's there in a review session that has general comments, and only then. A local review has none. The diff still opens on the first file, and the previous file from there (`F`, `[`) is the Conversation.

The page shows the review's title, then the description, then the other comments by when they were written. Each is in a box like a thread's, with the author in bold, how long ago, its kind (`description`, `review`) and the plugin's label, such as a review's `approved`. Text shows as [Markdown](#reading), as in a thread. The page scrolls like a file. `/` doesn't search it.

While it shows, `m` and `M` step through its comments, and past it they go from file to file, since they go between threads on code. `o` opens a comment on its host, and it's all the right-click menu offers there. The page is read-only for now: `r`, `x`, `C` and "Continue in editor" say so, and nothing is folded.

The status bar's and the panel title's counts are threads, without the general comments. The tree's row counts those.

## Opening a terminal

`space` then `.` (`open_terminal`), or the menu's `Open terminal here`, opens a new tab at the working tree's root in the terminal dffy runs in. Where that's all the terminal offers, it opens a window. dffy starts nothing in it but your shell, and its own screen stays as it was.

| Terminal | Found by | Opens |
|---|---|---|
| Rex | `TERM_PROGRAM=rex`, else `REX_SESSION` | a window in the session, through the `rex` in `REX_BIN_DIR`, else on `PATH` |
| herdr | `TERM_PROGRAM=herdr`, else `HERDR_ENV` | a tab in the workspace (`HERDR_BIN_PATH`, `HERDR_WORKSPACE_ID`) |
| tmux | `TERM_PROGRAM=tmux`, else `TMUX` | a window (`tmux new-window`) |
| zellij | `ZELLIJ` | a tab (`zellij action new-tab`) |
| WezTerm | `TERM_PROGRAM=WezTerm`, else `WEZTERM_PANE` | a tab (`wezterm cli spawn`) |
| kitty | `KITTY_WINDOW_ID` | a tab, with remote control on: `allow_remote_control` and `listen_on` set, so that kitty sets `KITTY_LISTEN_ON` |
| Ghostty | `TERM_PROGRAM=ghostty`, else `GHOSTTY_RESOURCES_DIR` | on macOS a tab, through AppleScript (Ghostty 1.3 or later); elsewhere a window (`ghostty +new-window`) |
| iTerm2 | `TERM_PROGRAM=iTerm.app`, else `ITERM_SESSION_ID` | a tab, through AppleScript; iTerm2 can't start a tab in a directory, so it types a `cd` there into the tab's shell |
| Terminal.app | `TERM_PROGRAM=Apple_Terminal` | a window (`open -a Terminal`) |
| Alacritty | `ALACRITTY_SOCKET` or `ALACRITTY_WINDOW_ID` | a window (`alacritty msg create-window`) |

The innermost terminal gets the tab, since a multiplexer's variables sit beside those of the terminal it runs in. A `TERM_PROGRAM` of Rex, herdr or tmux comes first. Then comes `ZELLIJ`, since zellij leaves the outer terminal's `TERM_PROGRAM` in place. Then come the other terminals' `TERM_PROGRAM`, then each one's own variables in the table's order. tmux in zellij and zellij in tmux look the same, and get a tmux window.

Otherwise `cd <the working tree>` goes on the clipboard, and the status bar says why: a terminal dffy doesn't know, one it can't drive from outside (kitty without remote control), or a command that failed.

## Phones and small screens

Under 16 rows or 100 columns, as on a phone, there's no frame. A title row and a status row are the only chrome, and every other row shows code. Under 100 columns the title becomes a file switcher, `‹ 3/12 name.rs ›`, with tap targets. A tap on an arrow moves between files, and a tap on the name opens the picker.

Most actions have a plain-letter key, and the command panel (`space`) reaches the rest with letters too, so a phone keyboard rarely needs ctrl or its symbol layer. Taps arrive as clicks and swipes as the wheel.

- Under 100 columns, the left panel opens over the whole diff instead of beside it. A tap on a file opens it and closes the tree. `enter` on a thread in the comments closes them on it.
- `o` finds a file by typing part of its name, without opening the tree.
- The command panel and the key list take the whole screen on narrow terminals.
- A remembered split layout shows unified there, without forgetting the split.
## Watch mode

`--watch` (or `w`) refreshes the view as files, the index or refs change. Every dffy process watching a repository shares one file watcher through `dffyd`, which starts on first use and exits when idle. Changes in directories git ignores that hold no tracked files never cause a refresh. [watch.md](watch.md) covers what's watched, the broker and the Linux watch limit.

## As git's pager

With `core.pager = dffy`, or `git log -p | dffy`, dffy reads the diff git writes and opens the same viewer, with each commit as a tree entry. Short diffs are printed, and input that isn't a diff goes to your pager unchanged. [git.md](git.md) covers the setup, `git add -p` and `diff.external`.

## Program status

dffy tells the terminal what it's doing with the [Program Status Protocol](https://superlogical.com/rex/docs/build/program-status) (OSC 7501). A terminal that supports it, such as Rex, can show it on a tab you aren't looking at. Terminals without support ignore it.

| Status | When |
|---|---|
| working | While the changes are listed, and while the review plugin fetches, updates or posts, with how far it got when the plugin counts it |
| done | Once a review is posted, until you press a key or click |
| error | When listing, refreshing or a plugin run fails, until you press a key or click |
| idle | Otherwise |

Quitting clears the status. tmux drops these reports, so inside tmux the terminal doesn't see them.

The window title, which most terminals show on the tab, names the working tree's directory and what dffy shows, such as `dffy · api · PR #7` or `dffy · api · uncommitted`. Quitting puts the previous title back on terminals that keep a title stack.

## Settings that stick

The viewer remembers these settings in `$XDG_STATE_HOME/dffy/state`: your last layout, wrapping, tree, status bar, indicators, modified lines, syntax diff, rendered view, highlighting, theme, mouse, line numbers, hunk headers, copy decorations, whole files (`X`, saved as `context`), ignored whitespace and watch mode.

- A flag (`--split`, `-U5`, `-w`, `--watch`...) wins for one run and isn't saved. What was remembered beats the config file.
- Printed output, the pager and `--color-only` ignore the state, so scripts always get the same output.
- Revealed gaps (`x`), the search, the tree's filter, viewed marks, the selection and where you are last only for the session.
- Under 100 columns, a remembered split shows unified and a remembered tree doesn't open over the diff. Both stay remembered.
- With watch mode remembered, `dffy` in a repository without changes opens and waits for some instead of printing that there are none.
- `remember = false` turns this off.
- Settings an older dffy doesn't know are kept when it writes the file.

`dffy config` prints every setting with where its value comes from. [config.md](config.md) lists every setting.

## Most used keys

| Key | Action |
|---|---|
| `space` | Command panel |
| `?` | Every key in effect, your remaps included |
| `j` `k` | Scroll; `ctrl+d`/`ctrl+u` half a screen, `g`/`G` top and bottom of the file |
| `n` `N` | Next or previous change, on into the next or previous file |
| `f` `F` | Next or previous file |
| `/` | Search the diff, or filter the tree when it has the keys ([Search](#search)) |
| `o` | Find a file by name |
| `x` `X` | 20 more lines of the highlighted `⋯ N unchanged lines` gap / every file whole or changes only |
| `t` `tab` | Show or hide the left panel / move the keys between it and the diff |
| `s` | Layout: split, unified, auto |
| `W` `d` `w` | Wrap long lines / syntax diff / watch mode |
| `r` | Rendered view, for file types with a renderer plugin (experimental, [plugins.md](plugins.md)) |
| `v` `y` `Y` | Select lines / copy them / copy the file's path |
| `A` `U` `D` | Stage, unstage or discard the selected lines or the hunk, on uncommitted changes ([Staging](#staging)) |
| `n` `N` `o` `t` `b` `a` `w` | In a file with conflicts to resolve: next or previous conflict / keep current, take incoming, keep both, take base / write the file ([Merge conflicts](#merge-conflicts)) |
| `e` | Open `$VISUAL` or `$EDITOR` at the line last tapped, else the top line |
| `c` `S` `m` `M` | Comment on the selection or the top line / suggest a change to it / next or previous comment thread ([Comments](#comments)) |
| `z` `Z` `i` | Fold a thread or all of them / every thread in the left panel, or the files again |
| `p` `P` `O` `R` | On a review a review plugin opened: update it in place / post your review / open another / since your last review ([Code host reviews](#code-host-reviews), experimental) |
| `'` | Only what changed since the latest checkpoint, such as an agent's turn, or the diff from before again ([Checkpoints](#checkpoints)) |
| `E` | Write the comment in `$EDITOR`: your draft in the focused thread, else a reply, else a comment on the line |
| `esc` | Step back: close a panel, end a search or filter, cancel a selection, leave a thread or the left panel |
| `q` | Quit |

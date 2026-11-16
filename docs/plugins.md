# Renderer plugins

**Experimental.** The protocol and the settings may change. dffy has no renderer built in, and none is installed by default.

A renderer plugin draws a file in its rendered form, such as a Markdown file with its headings, lists and tables, or a CSV file as rows of cells.

The plugin is a separate program:

1. dffy hands it both sides of a changed file.
2. It hands back each side as blocks of styled lines.
3. dffy diffs those blocks.

The viewer works as on any diff:

- changed blocks are tinted, and an edited paragraph shows the words that changed;
- a block that moved is marked moved (`»`);
- `n`/`N` go between changed blocks, `/` searches the rendered text, and `x`/`X` reveal unchanged blocks.

## Enabling one

1. **Install it.** Two plugins come from this repository:

   | Plugin | Draws |
   |---|---|
   | `dffy-render-markdown` | Markdown files, with ```` ```mermaid ```` fences drawn as diagrams. |
   | `dffy-render-mermaid` | Mermaid files (`.mmd`, `.mermaid`): [flowcharts and sequence diagrams](#the-mermaid-plugin) drawn in text. |

   Both plugins come with each dffy release as separate downloads:

   ```sh
   brew install --cask amalucelli/tap/dffy-render-markdown amalucelli/tap/dffy-render-mermaid     # or:
   curl -fsSL https://raw.githubusercontent.com/amalucelli/dffy/main/install.sh | sh -s -- --plugin markdown --plugin mermaid
   ```

   A plugin must be on `PATH`. [install.md](install.md#plugins) also covers `cargo install --git`, and from a clone `task install:plugins` puts both in `~/.local/bin`.

2. **Map file types to it** in the config file, by extension:

   ```
   render = md:markdown
   ```

   Each pair is `extension:name`, and the name picks the program `dffy-render-<name>` on `PATH`. List several pairs separated by spaces, e.g. `render = md:markdown markdown:markdown` to cover `.markdown` files too, or `render = md:markdown mmd:mermaid mermaid:mermaid` for Mermaid files.

3. **Press `r`** in the viewer on a file of that type, and `r` again to go back to the lines. The status bar says `rendered` while it's on.

   The choice is remembered (`rendered` in the state file) and applies to every file whose type has a renderer; other files keep their lines.

`dffy doctor` lists each configured renderer and whether its program is on `PATH`, with `!` when it isn't.

In the rendered view:

- Lines wrap between words whatever the `wrap` setting says, since renderers leave a paragraph on one line. A word wider than the pane breaks after a hyphen or slash, or wherever it must.
- The gutter numbers each block with the source line it starts on. Hunk headers (`@@ -a,b +c,d @@`) and `⋯ N unchanged lines` count source lines too, blank lines between blocks included.
- A copy (`v`, `y`, or a mouse drag) holds the source of every selected block. The rendered text isn't copied.
- `e` opens the editor at the source line of the block at the top.

When a file can't be rendered, its lines show and the status bar says why:

- no renderer for its type;
- the program isn't on `PATH`;
- the renderer failed;
- the file is binary or too large, or isn't UTF-8 text (it's never sent then).

A diff piped to dffy has only its hunks, so there's nothing to render.

## The Mermaid plugin

`dffy-render-mermaid` draws a Mermaid file's diagram with box characters, so its diff reads as a diagram. A file holds one diagram. The Markdown plugin draws ```` ```mermaid ```` fences the same way.

- **Flowcharts** (`graph`, `flowchart`; `TD`/`TB`, `BT`, `LR`, `RL`):
  - nodes with their labels, in shapes for `[rect]`, `(round)`, `([stadium])`, `((circle))`, `[[subroutine]]`, `[(cylinder)]`, `{diamond}`, `{{hexagon}}`, `>flag]` and `[/lean/]`, and a box for any other;
  - edges `-->`, `---`, `-.->`, `==>`, `--o`, `--x`, `<-->` and `~~~` (spacing only), labeled with `-->|text|` or `-- text -->`, chained (`A --> B --> C`) and grouped with `&`;
  - an edge from a node to itself as `↻` in the node;
  - `subgraph … end` as a titled frame. A frame that would overlap a node outside it or another frame is left out, and a line under the drawing lists its nodes instead.
  - `classDef`, `class`, `style`, `linkStyle` and `click` are skipped. An edge to a subgraph goes to a node of the subgraph's name.
- **Sequence diagrams** (`sequenceDiagram`):
  - a lifeline per `participant` or `actor` (an actor's box has round corners), with `as` aliases;
  - messages `->`, `-->`, `->>`, `-->>`, `-x`, `--x`, `-)`, `--)`, `<<->>` and `<<-->>`, a message to oneself as a loop, and `autonumber`;
  - notes `left of`, `right of` and `over` one or two participants;
  - `loop`, `alt`/`else`, `opt`, `par`/`and`, `critical`/`option` and `break` as titled frames.
  - `activate` and `deactivate` are skipped, as are `box` groups and `rect` highlights.
- **Anything else**, a diagram that doesn't parse, and a diagram past the size limits show as their source lines, a block per line, so their diff is a line diff. The limits: 300 nodes, 600 edges, 100 subgraphs, or 3000 nodes and edge bends in a flowchart; 60 participants or 2000 statements in a sequence diagram; a drawing over a million characters. Node ids are letters, digits and `_`; an id with `-` or `.` doesn't parse.

Blocks:

- A flowchart is one block. When it changes, the old and new drawings show side by side, and words that changed on a line are emphasized. An edge label edited within the room it had changes only its own line. An edit that needs more room, such as a longer node label, moves what's beside or after it, and those lines show as changed too.
- A sequence diagram is a block per statement: the participants, each message, each note, and each frame's top, divider and bottom. An added or edited message shows as that message alone. A message long enough to widen a column also changes every arrow that crosses that column.

Diagrams are fit to the pane's width by wrapping labels and messages. A diagram that's still wider keeps its width, and dffy wraps its lines.

## The protocol, version 1

A renderer is an executable named `dffy-render-<name>`. dffy runs it with no arguments, writes one JSON request to its stdin and closes it, then reads one JSON answer from its stdout.

### The request

```json
{
  "version": 1,
  "path": "docs/README.md",
  "old": "# Title\n\nOld text.\n",
  "new": "# Title\n\nNew text.\n",
  "width": 72,
  "background": "dark"
}
```

| Field | Meaning |
|---|---|
| `version` | The protocol version, `1`. |
| `path` | The file's path in the repository. |
| `old`, `new` | The two sides' whole text, or `null` for a side that doesn't exist (an added or deleted file). |
| `width` | About how many columns a pane has for text; wrap tables and other wide blocks to it. |
| `background` | `dark` or `light`: the terminal's background, for renderers that pick colors. |

### The answer

```json
{
  "version": 1,
  "old": [
    {"kind": "heading", "source": [0, 1], "lines": [[{"text": "Title", "style": ["heading"], "level": 1}]]},
    {"kind": "paragraph", "source": [2, 3], "lines": [["Old text."]]}
  ],
  "new": [
    {"kind": "heading", "source": [0, 1], "lines": [[{"text": "Title", "style": ["heading"], "level": 1}]]},
    {"kind": "paragraph", "source": [2, 3], "lines": [["New ", {"text": "text", "style": ["bold"]}, "."]]}
  ]
}
```

`old` and `new` are each a list of blocks, or `null` when the request's side was `null`. A block is a piece of the document that changes only whole: a heading, a paragraph, a list item, a table row.

| Block field | Meaning |
|---|---|
| `kind` | A word for what it is (`heading`, `paragraph`, `list-item`, `code`, `table-row`...). Any word does; it's part of the block's identity. |
| `source` | `[start, end]`: the lines of that side's text the block comes from, 0-based, `end` not included. It must lie within the side's lines. An empty range (`[n, n]`) is a block drawn from no source line, such as the blank line between two blocks: it gets no line number and is never marked moved. Make that blank line a block of its own rather than the last line of the block before it, or adding a block after that one changes it too. |
| `lines` | The block's lines on screen, each a list of spans. A span is a string, or an object with `text`, a `style` list and, for a link, its `url`. No span may hold a line break. |
| `language` | Optional, for a code block: the language of its text (`rust`, `py`, `sh`). dffy highlights it with its own highlighter when it knows the language. |

Span styles are names:

| Style | Drawn as |
|---|---|
| `bold`, `italic`, `underline`, `dim` | That attribute. |
| `code` | The theme's string color. |
| `link` | Underlined, in the theme's function color. Give the target in the span's `url`. |
| `heading` | Bold; with the span's `level` 1 also underlined, and levels 1 and 2 in the theme's keyword color. |
| `syntax-<role>` | A theme's syntax color: `syntax-keyword`, `syntax-string`, `syntax-comment`, `syntax-function`, `syntax-type`, `syntax-number`, `syntax-constant`, and the other roles in [themes.md](themes.md). |

- Unknown names and fields are ignored.
- Escape sequences aren't interpreted. Text is drawn as text, with control characters shown escaped.
- **Errors:** to fail, answer `{"version": 1, "error": "what went wrong"}`; dffy shows the message whatever the exit status. Any other answer counts only from a renderer that exits with status 0.

### How dffy diffs blocks

- **Identity:** a block's identity is its `kind`, its words with each one's style and link target, and its `source` lines, trailing whitespace aside.
- **Diff:** dffy diffs the two lists of identities with its line diff. A block is unchanged only when its source is; how the rendered text is spaced or wrapped doesn't count.
- **Changed blocks** are removed or added whole.
- **Pairing:** within each run of changed blocks, a removed block pairs with an added block of the same kind whose text is similar enough, by the test that pairs edited lines in a line diff.
- **Word emphasis:** in a pair, a word whose text, style or link target changed is emphasized.
- **Moved:** a changed block whose identity is also among the other side's changed blocks is marked moved.

### Versions

- Both the request and the answer carry `version`.
- dffy refuses an answer of a version it doesn't speak and shows the lines with a note. A renderer should answer a request of a version it doesn't speak with an error.
- A change that would break existing renderers bumps the version; new optional fields and new style names don't.

## Writing your own

A renderer can be written in any language. It reads all of stdin, renders each side that isn't `null`, and prints the answer.

- Blocks are the unit of change. Make each one as small as one change a reader would notice: a paragraph, an item, a row. A whole table as one block shows a one-cell edit as the whole table replaced.
- Leave paragraphs on one line. dffy wraps them, and word emphasis works best on a paragraph diffed as one line. Wrap only what has its own layout, like tables, to `width`.
- Answer within 500ms. dffy doesn't wait longer.
- Point `source` at the right lines. Copies and `e` use it.

This renderer shows CSV files as rows of cells. Save it as `dffy-render-csv`, make it executable, put it on `PATH`, and add `render = csv:csv` to the config:

```python
#!/usr/bin/env python3
"""dffy renderer for CSV: each row a block, cells between bars, the
header row bold. Assumes one row per line (no quoted line breaks)."""
import csv, io, json, sys

def blocks(text):
    if text is None:
        return None
    out = []
    for n, row in enumerate(csv.reader(io.StringIO(text))):
        line = []
        for i, cell in enumerate(row):
            if i:
                line.append({"text": " │ ", "style": ["dim"]})
            line.append({"text": cell, "style": ["bold"]} if n == 0 else cell)
        out.append({"kind": "row", "source": [n, n + 1], "lines": [line]})
    return out

request = json.load(sys.stdin)
if request.get("version") != 1:
    print(json.dumps({"version": 1, "error": "this renderer speaks protocol 1"}))
    sys.exit(1)
print(json.dumps({"version": 1, "old": blocks(request["old"]), "new": blocks(request["new"])}))
```

The Markdown plugin in `plugins/markdown` is a fuller example in Rust. It uses pulldown-cmark and draws:

- lists with bullets and numbers;
- quotes with a `│` bar;
- tables with box characters, wrapped to the width;
- links underlined, images as `[image: alt]`, and HTML dim;
- Mermaid fences through the Mermaid plugin's library.

The Mermaid plugin in `plugins/mermaid` lays out graphs itself, with no dependency beyond JSON and character widths.

## Performance and safety

### When renderers run

- Without a `render` setting, nothing about renderers happens.
- With one, dffy looks for a renderer's program on `PATH` only when a file of its type is to be shown rendered. That happens when you press `r`, or when such a file comes up with the rendered view remembered as on.
- The answer is kept for the session.

### The viewer never waits

- A renderer runs on dffy's worker threads; the file's lines show until its rendering arrives.
- Moving to another file, pressing `r` again or quitting stops a renderer still running, with every process it started.

### Timeouts and failures

- A renderer that takes longer than 500ms is killed with every process it started, and the file keeps its lines with a note.
- The same happens when a renderer crashes, prints something that isn't a valid answer, or answers in another protocol version.
- dffy doesn't retry a failed file until it changes or `r` turns rendering on again.

### Caching

Answers are kept in dffy's disk cache (`cache`), so reopening a file doesn't run the renderer again. The key covers:

- the renderer's name, and its program's modification time and size, so replacing the program invalidates its answers;
- both sides' contents;
- the width and the background.

### What can run

- Only programs you installed run, and only `dffy-render-<name>` for a name your config lists.
- Names are lowercase letters, digits, `-` and `_`, so the setting can't point at another path.
- Relative or empty `PATH` entries are skipped, so the repository being viewed can't supply a renderer.
- stderr is discarded, since it would land on the viewer's screen.
- Answers over 64MB are refused.

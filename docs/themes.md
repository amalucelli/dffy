# Themes

dffy ships 16 themes and reads your own from `~/.config/dffy/themes/`. Set one with `theme = dracula` in the config, or for one run:

```sh
dffy --theme dracula
```

## Picking a theme

`dffy themes` lists every theme, grouped into terminal palette, dark and light, each with a short sample diff. The current theme has a `*`.

In the viewer, open the command panel (`space`) and press `enter` on the Theme row. The list holds every theme, built-in and yours, and the screen previews each as you move. `enter` keeps one and `esc` goes back. `←`/`→` on the row step through themes without the list. The viewer remembers the choice ([Settings that stick](viewer.md#settings-that-stick)).

| Group | Themes |
| --- | --- |
| Terminal palette | `default`, `mono` |
| Dark | `github-dark`, `tokyo-night`, `catppuccin-mocha`, `gruvbox-dark`, `nord`, `dracula`, `one-dark`, `solarized-dark`, `rose-pine`, `kanagawa` |
| Light | `github-light`, `catppuccin-latte`, `gruvbox-light`, `solarized-light` |

An unknown name falls back to `default` with a warning that names the closest theme.

## The default theme

`default` uses only the 16 ANSI colors, so dffy draws in your terminal's palette. Its tints mix into the terminal's background, which dffy asks the terminal for. `background = dark` or `light` in the config skips the question.

`mono` uses no colors, only bold, dim, italic, underline and reverse.

The other built-ins are truecolor and use each theme's published palette. dffy doesn't paint their background, so pick one that matches your terminal. `dffy themes` draws each sample on its theme's background.

## Custom themes

A theme is a file in `~/.config/dffy/themes/` (`$XDG_CONFIG_HOME/dffy/themes/`). Its name is the file name, and you select it like a built-in. A file named like a built-in replaces it.

To change a few colors of a built-in, inherit from it and set only what differs:

```
inherit = dracula
added = #8aff80
file-header = bold underline #ff79c6
```

Keys you don't set keep the inherited theme's value, or `default`'s without `inherit`. `inherit` names a built-in, and the line can go anywhere in the file.

To start from a complete file, dump a built-in and edit the copy:

```sh
dffy themes --dump dracula > ~/.config/dffy/themes/mine
```

A line dffy can't use, such as an unknown key or a bad color, is skipped with a warning naming the file, the line and the closest key.

## Values

A value is space-separated words, in any order:

- **A color** sets the text color. It's a name (`black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `white`, or the same with `bright-`, like `bright-blue`) or `#rrggbb`. Names use the terminal's palette. `#rrggbb` needs a terminal with 24-bit color.
- **`on <color>`** sets the background.
- **Attributes:** `bold`, `dim`, `italic`, `underline`, `reverse`.
- **Nothing** (`context =`) leaves the terminal's own style.

For example `bold #f8f8f2 on #44475a`.

## Roles

| Key | What it colors |
| --- | --- |
| `background` | Not drawn. The background the theme is made for: tints mix into it, and it sorts the theme into dark or light. `#rrggbb` or empty. |
| `context` | Unchanged lines. |
| `added`, `removed` | Changed lines without syntax colors, their signs and bars, the `+N`/`-N` counts, and the `A` and `D` letters in the file tree. `removed` also colors the `U` of a conflicted file. |
| `added-emph`, `removed-emph` | The changed words inside a changed line. |
| `added-line`, `removed-line` | Changed lines with syntax colors, which replace this text color. Their background is the line's tint. |
| `line-number` | Line numbers of unchanged lines. |
| `added-number`, `removed-number` | Line numbers of changed lines. Their background is the gutter's tint. |
| `modified` | A removed line and the added line it was edited into (with `modified-lines = changed`, the default), without syntax colors or tints. Also their signs, bars and marks in the overview column. Their changed words keep `removed-emph` and `added-emph`. The built-ins use a blue (`blue` in `default`, `underline` in `mono`). |
| `modified-line` | Modified lines with syntax colors or tints. Their background is the line's tint. |
| `modified-number` | Line numbers of modified lines. Their background is the gutter's tint. |
| `moved` | Lines the syntax diff found moved whole: their text, `»` sign and numbers. |
| `conflict-marker` | Added conflict markers (`<<<<<<<`, `=======`, `>>>>>>>` and diff3's base marker) in a conflicted file. Their background is the added line's tint. |
| `file-header` | File names in headers, the `‹ 3/12 name ›` switcher and panel titles. |
| `hunk-header` | `@@` lines. |
| `commit-header` | Commit lines from `dffy show` and `git log -p`, and commit ids in the tree. |
| `error`, `warn`, `ok`, `hint` | Messages. `warn` also colors modified files (`M`) in the tree and conflict markers that were in the file before the merge. `hint` also colors renamed and copied files (`R`, `C`), the border of the focused panel, keys in the panels, and filter matches. |
| `dim` | Secondary text: borders, separators, `⋯ unchanged lines`, untracked files (`?`), files marked viewed, and lines the syntax diff found only reformatted. |
| `search-match`, `search-current` | Search matches, and the one `n`/`N` went to. |
| `status-tag` | The chip in the status bar naming what's compared (`uncommitted`, `v6.0..v6.1`). |
| `selection` | The selected row of the file tree, the picker and the command panel. Its background and attributes go over the row. Its text color applies only where the row's text has none, so status letters and counts keep their colors. Its background also tints a folded review thread that has the focus. |
| `selection-inactive` | The tree's selected row while the diff has the focus. Its background also tints a folded review thread. |
| `landed` | The line a jump landed on (a definition, back and forward, the control API's `view/open`), until the view moves. Its background and attributes go over the row, and its text color over the row's when it sets one; the `❯` in the line's gutter takes the whole style. The built-ins use `black on bright-blue` in `default`, `bold underline` in `mono`, and the palette's cyan mixed into its background in the others. |
| `syntax-keyword` | Keywords. |
| `syntax-string` | Strings. |
| `syntax-escape` | Escape sequences in strings. |
| `syntax-comment` | Comments. |
| `syntax-function` | Function and method names. |
| `syntax-type` | Types. |
| `syntax-number` | Numbers. |
| `syntax-constant` | Constants, booleans and characters. |
| `syntax-operator` | Operators. |
| `syntax-punctuation` | Brackets and delimiters. |
| `syntax-property` | Fields and properties. |
| `syntax-variable-builtin` | `self`, `this` and similar. |
| `syntax-attribute` | Attributes, annotations and decorators. |
| `syntax-tag` | HTML and XML tags. |
| `syntax-label` | Labels. |

Syntax colors set only the text color and attributes. The line's background stays.

## Tints

With `indicators = background`, the default on truecolor terminals, changed lines get a tinted background. Each tint is a hue mixed into the theme's `background`, or into the terminal's background for themes without one.

| Lines | Hue |
|---|---|
| Added | Green |
| Removed | Red |
| Modified | The hue of the theme's `modified` color when that's a blue or an indigo, otherwise a fixed blue |
| Moved | Purple |

| Where | Strength |
|---|---|
| The line | 15% |
| Its line-number cell | 25% |
| Changed words | 35% |

- Red, green and modified tints have the same lightness on any background.
- Line numbers that don't reach a 4.5:1 contrast on their tint are drawn in a lighter or darker shade of the same hue.
- Under tints, `reverse` on `added-emph` or `removed-emph` becomes the stronger tint, with the text in the theme's `context` color or its syntax color.

To use your own diff colors, give the roles a background. A role with its own background keeps it:

```
inherit = github-dark
added-line = on #12261e
removed-line = on #25171c
added-number = #3fb950 on #1b4721
removed-number = #f85149 on #4c1a1e
added-emph = on #238636
removed-emph = on #b62324
```

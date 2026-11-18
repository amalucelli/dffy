//! The config file: Ghostty-style `key = value` lines at
//! `$XDG_CONFIG_HOME/dffy/config`. Every behavior flag has a key and flags
//! win over the file. dffy only ever reads this directory, so it can live in
//! a dotfiles repository.

use std::path::{Path, PathBuf};

use clap::ValueEnum;
use dffy_core::diff::Whitespace;

use crate::keys::{self, Keys};
use crate::plugin::Renderers;
use crate::render::{Canvas, escape_lines};
use crate::theme::Role;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ColorWhen {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Paging {
    Auto,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum View {
    /// Split when the terminal is at least `split-min-width` columns wide.
    Auto,
    Split,
    Unified,
}

/// Unchanged lines shown around changes: a number, or the whole file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Context {
    Lines(usize),
    Full,
}

impl Context {
    /// Hunks show this many lines around changes. Without hiding anything,
    /// the viewer still splits the file at the default context, so `X` can
    /// go back to changes only.
    pub(crate) fn hunk_lines(self, viewer: bool) -> usize {
        match self {
            Self::Lines(n) => n,
            Self::Full if viewer => 3,
            // Large enough to join every hunk, small enough not to overflow.
            Self::Full => usize::MAX / 4,
        }
    }
}

impl std::str::FromStr for Context {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        if value == "full" {
            return Ok(Self::Full);
        }
        value
            .parse()
            .map(Context::Lines)
            .map_err(|_| format!("'{value}' is not a number of lines or full"))
    }
}

impl std::fmt::Display for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lines(n) => write!(f, "{n}"),
            Self::Full => f.write_str("full"),
        }
    }
}

/// How changed lines are marked besides their color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Indicators {
    /// `Background` on a truecolor terminal, `Bars` otherwise.
    Auto,
    /// A tint behind the line, a stronger one behind its numbers.
    Background,
    /// A colored bar in the gutter.
    Bars,
    /// `+` and `-` signs before the text.
    Classic,
    /// Color alone.
    None,
}

/// What `dffy` without revisions compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum DefaultDiff {
    /// Everything uncommitted: `HEAD` against the working tree, staged,
    /// unstaged and untracked.
    All,
    /// The working tree against the index, as `git diff` (`--unstaged`).
    Unstaged,
}

/// Whether the viewer starts with the file tree shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum TreeMode {
    /// Shown on terminals at least 100 columns wide, beside the diff.
    Auto,
    /// Shown on every terminal: over the diff on narrow ones.
    Show,
    Hide,
}

/// Which edge of the viewer's frame holds the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum StatusBar {
    Bottom,
    Top,
}

/// How a removed line and the added line it was edited into are colored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ModifiedLines {
    /// Both in the modified color, their changed tokens red and green.
    Changed,
    /// Red and green, as wholly removed and added lines.
    AddedRemoved,
}

/// Whether the terminal's background is dark or light, for the tints of
/// `indicators = background`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Background {
    /// Dark, unless the viewer's query of the terminal says light.
    Auto,
    Dark,
    Light,
}

/// A problem in a config or theme file. Never fatal: the line is skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Warning {
    pub file: Option<PathBuf>,
    pub line: usize,
    pub message: String,
    pub hint: Option<String>,
}

impl Warning {
    pub(crate) fn in_file(mut self, path: &Path) -> Self {
        self.file = Some(path.to_owned());
        self
    }

    pub(crate) fn render(&self, c: &mut Canvas) {
        c.push(Role::Warn, "warning:");
        let location = match (&self.file, self.line) {
            (Some(file), 0) => format!(" {}: ", file.display()),
            (Some(file), n) => format!(" {}:{n}: ", file.display()),
            (None, 0) => " ".to_owned(),
            (None, n) => format!(" line {n}: "),
        };
        // Values from the file and from flags are echoed in the message.
        c.push(Role::Plain, &escape_lines(&location));
        c.push(Role::Plain, &escape_lines(&self.message));
        c.end_line();
        if let Some(hint) = &self.hint {
            c.push(Role::Hint, "hint:");
            c.push(Role::Plain, &format!(" {}", escape_lines(hint)));
            c.end_line();
        }
    }
}

/// A value a config or theme line can't take, with what to do instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Invalid {
    pub message: String,
    pub hint: Option<String>,
}

impl From<String> for Invalid {
    fn from(message: String) -> Self {
        Self {
            message,
            hint: None,
        }
    }
}

pub(crate) struct Key {
    pub name: &'static str,
    pub doc: &'static str,
    pub default: &'static str,
}

/// Every config key. Flags that select what to diff (`--staged`, `--stat`,
/// `-R`, ...) are per-invocation and have no key.
pub(crate) const KEYS: &[Key] = &[
    Key {
        name: "color",
        doc: "Color output: auto, always or never. With auto, NO_COLOR and CLICOLOR_FORCE apply.",
        default: "auto",
    },
    Key {
        name: "paging",
        doc: "Page output on a terminal: auto or never.",
        default: "auto",
    },
    Key {
        name: "pager",
        doc: "Pager command, run through sh. Empty uses $PAGER, then less -RFX. DFFY_PAGER overrides it.",
        default: "",
    },
    Key {
        name: "theme",
        doc: "Theme name: a built-in or a file in the themes directory next to this file.",
        default: "default",
    },
    Key {
        name: "view",
        doc: "Diff layout: auto, split or unified. Auto splits on terminals at least split-min-width wide.",
        default: "auto",
    },
    Key {
        name: "indicators",
        doc: "How changed lines are marked besides their color: background (a tint behind the line and a stronger one behind its numbers), bars (a colored bar in the gutter), classic (+ and - signs) or none. Auto picks background on truecolor terminals (COLORTERM=truecolor or 24bit) and bars otherwise. Without color, classic signs are always used so the output stays a readable diff.",
        default: "auto",
    },
    Key {
        name: "modified-lines",
        doc: "How a changed line is colored when it pairs with the line it was edited into: changed (both lines in the modified color, theme keys `modified`, `modified-line` and `modified-number`, with only the edited tokens in red and green) or added-removed (a red line and a green one, as for lines wholly removed or added). Lines without a partner are red or green either way. --color-only keeps red and green.",
        default: "changed",
    },
    Key {
        name: "background",
        doc: "The terminal's background for the tints of indicators = background: auto, dark or light. Auto starts dark and asks the terminal (OSC 11), switching to light tints if it answers within 200ms; printed output uses dark.",
        default: "auto",
    },
    Key {
        name: "split-min-width",
        doc: "Terminal width in columns from which view = auto picks the split layout.",
        default: "140",
    },
    Key {
        name: "wrap",
        doc: "Wrap long lines instead of truncating them.",
        default: "false",
    },
    Key {
        name: "tab-width",
        doc: "Columns per tab stop.",
        default: "4",
    },
    Key {
        name: "context",
        doc: "Lines of context around each change (-U), or full for the whole file with changes inline. In the viewer, `X` switches every file between the two and is remembered; `x` reveals hidden lines in one file, for that run.",
        default: "3",
    },
    Key {
        name: "ignore-all-space",
        doc: "Ignore all whitespace when comparing lines (-w). The command panel's `W` toggles it in the viewer.",
        default: "false",
    },
    Key {
        name: "ignore-space-change",
        doc: "Ignore changes in the amount of whitespace (-b). The command panel's `W` toggles between ignoring all whitespace and none.",
        default: "false",
    },
    Key {
        name: "default-diff",
        doc: "What dffy without revisions shows: all (everything not committed: HEAD against the working tree, with staged, unstaged and untracked files, each file marked by its staging) or unstaged (the working tree against the index, as plain git diff, which hides staged and untracked files; the same as --unstaged). --staged and revisions are unaffected.",
        default: "all",
    },
    Key {
        name: "renames",
        doc: "Detect renames as git diff -M does. Printed output waits for the rename pass when there are both added and deleted files; the viewer shows them unpaired first.",
        default: "true",
    },
    Key {
        name: "highlight",
        doc: "Syntax-highlight code in diffs (with color on). Files over 512KB are never highlighted.",
        default: "true",
    },
    Key {
        name: "syntax-diff",
        doc: "Diff code by syntax: emphasize only changed tokens (reordered arguments included), dim code that was only reformatted (~) and mark whole statements or items that moved (», theme key `moved`). `d` toggles it in the viewer. Files over 2MB or in unknown languages keep the line diff.",
        default: "false",
    },
    Key {
        name: "render",
        doc: "Experimental: renderer plugins by file extension, as space-separated extension:name pairs (md:markdown csv:csv). A file of a listed type can show rendered in the viewer, with `r`, by the program dffy-render-<name> on PATH, which is looked up and run only then. See docs/plugins.md.",
        default: "",
    },
    Key {
        name: "rendered",
        doc: "Show files that have a renderer (see render) rendered in the viewer, diffed block by block, instead of as lines; `r` toggles it.",
        default: "false",
    },
    Key {
        name: "review",
        doc: "Experimental: the review plugin, the program dffy-review-<name> on PATH. It adds the command it names to open one of the code host's reviews in the viewer, and plain `dffy` asks it for the branch's; empty for none, and no such command. --review <name> sets it for one run. See docs/plugins.md.",
        default: "",
    },
    Key {
        name: "watch",
        doc: "Start the viewer in watch mode, refreshing as files, the index or refs change; `w` toggles it. With nothing to show, a watching viewer opens and waits for changes instead of printing that there are none.",
        default: "false",
    },
    Key {
        name: "tree",
        doc: "Whether the viewer starts with the file tree: auto shows it on terminals at least 100 columns wide, show also opens it on narrower ones, over the diff, and hide starts without it. `t` and `T` toggle it; a `show` remembered from a wide terminal doesn't open it over the diff on a narrow one.",
        default: "auto",
    },
    Key {
        name: "tree-width",
        doc: "The tree's width in columns, or auto for 28% of the terminal (24 to 60 columns). `+` and `-` in the tree change it.",
        default: "auto",
    },
    Key {
        name: "status-bar",
        doc: "Where the viewer shows what's compared, the position, the modes and the key hints: bottom (in the frame's bottom border) or top (in its top border, after the file's name). The command panel's `b` switches it.",
        default: "bottom",
    },
    Key {
        name: "line-numbers",
        doc: "Show line numbers, in the viewer and in printed diffs. Without them the change marks stay.",
        default: "true",
    },
    Key {
        name: "hunk-headers",
        doc: "Show the @@ -a,b +c,d @@ numbers of hunks; without them a hunk shows only its function and the ⋯ N unchanged lines before it.",
        default: "true",
    },
    Key {
        name: "copy-decorations",
        doc: "Copies from the viewer include each line's number and its +/- sign, for pasting into a review, rather than only the code.",
        default: "false",
    },
    Key {
        name: "remember",
        doc: "Remember the view choices made in the viewer (layout, wrap, the tree and its width, the status bar, indicators, modified lines, syntax diff, rendered view, highlighting, theme, mouse, line numbers, hunk headers, copy decorations, whole files, ignored whitespace, watch mode) for the next run, in $XDG_STATE_HOME/dffy/state. A choice made in the viewer beats this file in the viewer; a flag beats both, for that run only. Printed output never takes them. A remembered split layout shows unified under 100 columns; --split doesn't.",
        default: "true",
    },
    Key {
        name: "tips",
        doc: "Show the one-time tip about the command panel on the first run of the viewer.",
        default: "true",
    },
    Key {
        name: "mouse",
        doc: "Take mouse input in the viewer: clicks, the wheel, and dragging over code to select and copy it. `M` in the command panel turns it off for the terminal's own selection (many terminals, Ghostty included, also select natively with shift held while dragging).",
        default: "true",
    },
    Key {
        name: "cache",
        doc: "Keep results that are slow to compute (renames, highlighting of large files) in $XDG_CACHE_HOME/dffy.",
        default: "true",
    },
    Key {
        name: "cache-size",
        doc: "Size the disk cache is trimmed to, least recently used first: bytes, or a number with K, M or G.",
        default: "256M",
    },
];

/// Effective settings: defaults, then the config file, then flags.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent settings, each its own config key"
)]
pub(crate) struct Config {
    pub color: ColorWhen,
    pub paging: Paging,
    pub pager: String,
    pub theme: String,
    pub view: View,
    pub indicators: Indicators,
    pub modified_lines: ModifiedLines,
    pub background: Background,
    pub split_min_width: usize,
    pub wrap: bool,
    pub tab_width: usize,
    pub context: Context,
    pub whitespace: Whitespace,
    pub default_diff: DefaultDiff,
    pub renames: bool,
    pub highlight: bool,
    pub syntax_diff: bool,
    pub render: Renderers,
    pub rendered: bool,
    /// The review plugin's name; `None` without one.
    pub review: Option<String>,
    pub watch: bool,
    pub tree: TreeMode,
    /// `None` follows the terminal's width.
    pub tree_width: Option<usize>,
    pub status_bar: StatusBar,
    pub line_numbers: bool,
    pub hunk_headers: bool,
    pub copy_decorations: bool,
    pub remember: bool,
    pub tips: bool,
    pub mouse: bool,
    pub cache: bool,
    pub cache_size: u64,
    /// From `keybind` lines, which can repeat; not in [`KEYS`].
    pub keys: Keys,
}

impl Default for Config {
    #[expect(
        clippy::expect_used,
        reason = "the defaults are constants in KEYS, and every test that builds a Config parses all of them"
    )]
    fn default() -> Self {
        let mut config = Self {
            color: ColorWhen::Auto,
            paging: Paging::Auto,
            pager: String::new(),
            theme: String::new(),
            view: View::Auto,
            indicators: Indicators::Auto,
            modified_lines: ModifiedLines::Changed,
            background: Background::Auto,
            split_min_width: 0,
            wrap: false,
            tab_width: 0,
            context: Context::Lines(0),
            whitespace: Whitespace::Exact,
            default_diff: DefaultDiff::All,
            renames: true,
            highlight: true,
            syntax_diff: false,
            render: Renderers::default(),
            rendered: false,
            review: None,
            watch: false,
            tree: TreeMode::Auto,
            tree_width: None,
            status_bar: StatusBar::Bottom,
            line_numbers: true,
            hunk_headers: true,
            copy_decorations: false,
            remember: true,
            tips: true,
            mouse: true,
            cache: true,
            cache_size: 0,
            keys: Keys::default(),
        };
        for key in KEYS {
            config
                .set(key.name, key.default)
                .expect("built-in defaults are valid");
        }
        config
    }
}

impl Config {
    /// Sets `key` from its config-file form. An empty value restores the
    /// default.
    pub(crate) fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        if value.is_empty()
            && let Some(k) = KEYS.iter().find(|k| k.name == key)
            && !k.default.is_empty()
        {
            return self.set(key, k.default);
        }
        match key {
            "color" => self.color = choice(value)?,
            "paging" => self.paging = choice(value)?,
            "pager" => value.clone_into(&mut self.pager),
            "theme" => value.clone_into(&mut self.theme),
            "view" => self.view = choice(value)?,
            "indicators" => self.indicators = choice(value)?,
            "modified-lines" => self.modified_lines = choice(value)?,
            "background" => self.background = choice(value)?,
            "split-min-width" => self.split_min_width = number(value, 0)?,
            "wrap" => self.wrap = boolean(value)?,
            "tab-width" => self.tab_width = number(value, 1)?,
            "context" => self.context = value.parse()?,
            "ignore-all-space" => {
                self.whitespace = match (boolean(value)?, self.whitespace) {
                    (true, _) => Whitespace::IgnoreAll,
                    (false, Whitespace::IgnoreAll) => Whitespace::Exact,
                    (false, other) => other,
                };
            }
            "ignore-space-change" => {
                self.whitespace = match (boolean(value)?, self.whitespace) {
                    (true, Whitespace::IgnoreAll) => Whitespace::IgnoreAll,
                    (true, _) => Whitespace::IgnoreChange,
                    (false, Whitespace::IgnoreChange) => Whitespace::Exact,
                    (false, other) => other,
                };
            }
            "default-diff" => self.default_diff = choice(value)?,
            "renames" => self.renames = boolean(value)?,
            "highlight" => self.highlight = boolean(value)?,
            "syntax-diff" => self.syntax_diff = boolean(value)?,
            "render" => self.render = Renderers::parse(value)?,
            "rendered" => self.rendered = boolean(value)?,
            "review" if value.is_empty() => self.review = None,
            "review" => {
                crate::plugin::check_name(value)?;
                self.review = Some(value.to_owned());
            }
            "watch" => self.watch = boolean(value)?,
            "tree" => self.tree = choice(value)?,
            "tree-width" => {
                self.tree_width = match value {
                    "auto" => None,
                    n => Some(number(n, 1).map_err(|e| format!("{e}, or auto"))?),
                };
            }
            "status-bar" => self.status_bar = choice(value)?,
            "line-numbers" => self.line_numbers = boolean(value)?,
            "hunk-headers" => self.hunk_headers = boolean(value)?,
            "copy-decorations" => self.copy_decorations = boolean(value)?,
            "remember" => self.remember = boolean(value)?,
            "tips" => self.tips = boolean(value)?,
            "mouse" => self.mouse = boolean(value)?,
            "cache" => self.cache = boolean(value)?,
            "cache-size" => self.cache_size = size(value)?,
            _ => return Err(format!("unknown key '{key}'")),
        }
        Ok(())
    }

    /// `key`'s value in config-file form.
    pub(crate) fn get(&self, key: &str) -> String {
        match key {
            "color" => choice_name(&self.color),
            "paging" => choice_name(&self.paging),
            "pager" => self.pager.clone(),
            "theme" => self.theme.clone(),
            "view" => choice_name(&self.view),
            "indicators" => choice_name(&self.indicators),
            "modified-lines" => choice_name(&self.modified_lines),
            "background" => choice_name(&self.background),
            "split-min-width" => self.split_min_width.to_string(),
            "wrap" => self.wrap.to_string(),
            "tab-width" => self.tab_width.to_string(),
            "context" => self.context.to_string(),
            "ignore-all-space" => (self.whitespace == Whitespace::IgnoreAll).to_string(),
            "ignore-space-change" => (self.whitespace == Whitespace::IgnoreChange).to_string(),
            "default-diff" => choice_name(&self.default_diff),
            "renames" => self.renames.to_string(),
            "highlight" => self.highlight.to_string(),
            "syntax-diff" => self.syntax_diff.to_string(),
            "render" => self.render.to_string(),
            "rendered" => self.rendered.to_string(),
            "review" => self.review.clone().unwrap_or_default(),
            "watch" => self.watch.to_string(),
            "tree" => choice_name(&self.tree),
            "tree-width" => self
                .tree_width
                .map_or_else(|| "auto".to_owned(), |n| n.to_string()),
            "status-bar" => choice_name(&self.status_bar),
            "line-numbers" => self.line_numbers.to_string(),
            "hunk-headers" => self.hunk_headers.to_string(),
            "copy-decorations" => self.copy_decorations.to_string(),
            "remember" => self.remember.to_string(),
            "tips" => self.tips.to_string(),
            "mouse" => self.mouse.to_string(),
            "cache" => self.cache.to_string(),
            "cache-size" => size_name(self.cache_size),
            _ => String::new(),
        }
    }

    /// Applies a config file's text, collecting a warning for each bad line.
    /// Returns the keys it set.
    pub(crate) fn apply_source(
        &mut self,
        source: &str,
        warnings: &mut Vec<Warning>,
    ) -> Vec<&'static str> {
        let names: Vec<&'static str> = KEYS.iter().map(|k| k.name).chain([KEYBIND]).collect();
        let mut set = Vec::new();
        parse_lines(source, &names, warnings, |i, value| match names[i] {
            KEYBIND => self.keys.bind(value),
            name => {
                self.set(name, value)?;
                set.push(name);
                Ok(())
            }
        });
        set
    }

    /// Lays `values` over these settings as coming from `source`, noting
    /// it in `sources`, except over a flag's. Values that don't parse are
    /// skipped: they come from a file dffy writes, not from someone to warn.
    pub(crate) fn layer(
        &mut self,
        sources: &mut Sources,
        values: &[(String, String)],
        source: Source,
    ) {
        for (key, value) in values {
            if let Some(k) = KEYS.iter().find(|k| k.name == key)
                && sources.get(k.name) != Source::Flag
                && self.set(k.name, value).is_ok()
            {
                sources.set(k.name, source);
            }
        }
    }

    /// Prints `key = value` for every key with where its value comes from,
    /// then the `keybind` lines that turn the default bindings into these.
    pub(crate) fn render(&self, sources: &Sources, c: &mut Canvas) {
        for key in KEYS {
            c.push(Role::Hint, key.name);
            c.push(Role::Dim, " = ");
            c.push(Role::Plain, &self.get(key.name));
            c.push(Role::Dim, &format!("  # {}", sources.get(key.name).name()));
            c.end_line();
        }
        self.keys.render_changes(c);
    }
}

/// Where a setting's value comes from, lowest precedence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Source {
    Default,
    Config,
    /// Remembered from the viewer (see [`crate::state`]).
    LastUsed,
    /// This run's flag; never remembered.
    Flag,
}

impl Source {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Config => "config",
            Self::LastUsed => "last used",
            Self::Flag => "flag",
        }
    }
}

/// The source of each key not at its default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Sources(Vec<(&'static str, Source)>);

impl Sources {
    pub(crate) fn set(&mut self, key: &'static str, source: Source) {
        self.0.retain(|(k, _)| *k != key);
        self.0.push((key, source));
    }

    pub(crate) fn get(&self, key: &str) -> Source {
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map_or(Source::Default, |&(_, s)| s)
    }
}

const KEYBIND: &str = "keybind";

/// Prints every key with its doc and default, as a commented config file,
/// then every action with its default keys.
pub(crate) fn render_defaults(c: &mut Canvas) {
    for (i, key) in KEYS.iter().enumerate() {
        if i > 0 {
            c.end_line();
        }
        c.line(Role::Dim, &format!("# {}", key.doc));
        c.push(Role::Hint, key.name);
        c.push(Role::Dim, " = ");
        c.push(Role::Plain, key.default);
        c.end_line();
    }
    c.end_line();
    keys::render_defaults(c);
}

fn choice<T: ValueEnum>(value: &str) -> Result<T, String> {
    T::from_str(value, false).map_err(|_| {
        let names: Vec<String> = T::value_variants().iter().map(choice_name).collect();
        format!("'{value}' is not one of {}", names.join(", "))
    })
}

pub(crate) fn choice_name<T: ValueEnum>(value: &T) -> String {
    value
        .to_possible_value()
        .map(|p| p.get_name().to_owned())
        .unwrap_or_default()
}

fn number(value: &str, min: usize) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(n) if n >= min => Ok(n),
        _ => Err(format!("'{value}' is not a whole number of at least {min}")),
    }
}

const SIZE_UNITS: [(char, u32); 3] = [('G', 30), ('M', 20), ('K', 10)];

/// A byte count: digits, optionally followed by K, M or G (powers of 1024).
fn size(value: &str) -> Result<u64, String> {
    let bad = || format!("'{value}' is not a size like 256M, 1G or 1048576");
    let (digits, shift) = match value.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => {
            let unit = c.to_ascii_uppercase();
            let &(_, shift) = SIZE_UNITS
                .iter()
                .find(|(u, _)| *u == unit)
                .ok_or_else(bad)?;
            (&value[..i], shift)
        }
        _ => (value, 0),
    };
    let n: u64 = digits.trim().parse().map_err(|_| bad())?;
    n.checked_mul(1 << shift).ok_or_else(bad)
}

/// A byte count in the largest unit that divides it.
fn size_name(bytes: u64) -> String {
    SIZE_UNITS
        .iter()
        .find(|&&(_, shift)| bytes != 0 && bytes.is_multiple_of(1 << shift))
        .map_or_else(
            || bytes.to_string(),
            |&(unit, shift)| format!("{}{unit}", bytes >> shift),
        )
}

fn boolean(value: &str) -> Result<bool, String> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("'{value}' is not true or false")),
    }
}

/// Splits `key = value` lines, skipping blanks and `#` comments, and calls
/// `apply` with the index of the key in `keys`. Unknown keys, malformed
/// lines and values `apply` rejects become warnings with a hint.
pub(crate) fn parse_lines(
    source: &str,
    keys: &[&str],
    warnings: &mut Vec<Warning>,
    mut apply: impl FnMut(usize, &str) -> Result<(), Invalid>,
) {
    for (n, raw) in source.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut warn = |message: String, hint: Option<String>| {
            warnings.push(Warning {
                file: None,
                line: n + 1,
                message,
                hint,
            });
        };
        let Some((key, value)) = line.split_once('=') else {
            warn(
                format!("expected 'key = value', found '{line}'"),
                Some("comments start with '#'".to_owned()),
            );
            continue;
        };
        // `dffy config` notes each value's source after two spaces and `#`.
        let value = value.split_once("  # ").map_or(value, |(v, _)| v);
        let (key, value) = (key.trim(), value.trim());
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        let Some(index) = keys.iter().position(|k| *k == key) else {
            let hint = dffy_core::suggest::closest(key, keys.iter().copied())
                .map(|close| format!("did you mean '{close}'?"));
            warn(format!("unknown key '{key}'"), hint);
            continue;
        };
        if let Err(Invalid { message, hint }) = apply(index, value) {
            warn(format!("{key}: {message}"), hint);
        }
    }
}

/// `$XDG_CONFIG_HOME/dffy`, falling back to `~/.config/dffy`.
pub(crate) fn config_dir() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    let base =
        xdg.or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("dffy"))
}

pub(crate) struct Loaded {
    pub config: Config,
    pub sources: Sources,
    /// The config file, when one exists.
    pub path: Option<PathBuf>,
    pub warnings: Vec<Warning>,
}

pub(crate) fn load() -> Loaded {
    let mut loaded = Loaded {
        config: Config::default(),
        sources: Sources::default(),
        path: None,
        warnings: Vec::new(),
    };
    let Some(path) = config_dir().map(|d| d.join("config")) else {
        return loaded;
    };
    match std::fs::read_to_string(&path) {
        Ok(source) => {
            let mut warnings = Vec::new();
            for key in loaded.config.apply_source(&source, &mut warnings) {
                loaded.sources.set(key, Source::Config);
            }
            loaded.warnings = warnings.into_iter().map(|w| w.in_file(&path)).collect();
            loaded.path = Some(path);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => loaded.warnings.push(Warning {
            file: Some(path),
            line: 0,
            message: format!("can't read the config file: {e}"),
            hint: None,
        }),
    }
    loaded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> (Config, Vec<Warning>) {
        let mut config = Config::default();
        let mut warnings = Vec::new();
        config.apply_source(source, &mut warnings);
        (config, warnings)
    }

    #[test]
    fn every_key_round_trips_its_default() {
        let config = Config::default();
        for key in KEYS {
            assert_eq!(config.get(key.name), key.default, "{}", key.name);
        }
    }

    #[test]
    fn parses_values_comments_and_quotes() {
        let (config, warnings) = parse(
            "# comment\n\ncolor = never\npager = \"less -S\"\nwrap=true\n  tab-width = 8  \n-w\n",
        );
        assert_eq!(config.color, ColorWhen::Never);
        assert_eq!(config.pager, "less -S");
        assert!(config.wrap);
        assert_eq!(config.tab_width, 8);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn empty_value_restores_the_default() {
        let (config, warnings) = parse("theme = mono\ntheme =\n");
        assert_eq!(warnings, []);
        assert_eq!(config.theme, "default");
    }

    #[test]
    fn whitespace_keys_combine_like_git_flags() {
        let (config, _) = parse("ignore-space-change = true\n");
        assert_eq!(config.whitespace, Whitespace::IgnoreChange);
        let (config, _) = parse("ignore-space-change = true\nignore-all-space = true\n");
        assert_eq!(config.whitespace, Whitespace::IgnoreAll);
    }

    #[test]
    fn cache_sizes_take_units() {
        let (config, warnings) = parse("cache-size = 1g\n");
        assert_eq!((config.cache_size, warnings.len()), (1 << 30, 0));
        assert_eq!(config.get("cache-size"), "1G");
        let (config, _) = parse("cache-size = 1000\n");
        assert_eq!(config.get("cache-size"), "1000");
        let (config, warnings) = parse("cache-size = lots\ncache-size = 5T\n");
        assert_eq!((config.cache_size, warnings.len()), (256 << 20, 2));
    }

    #[test]
    fn bad_lines_become_warnings_with_hints() {
        let (config, warnings) =
            parse("colour = always\nview = sideways\ntab-width = 0\ncontext = many\njust words\n");
        assert_eq!(config, Config::default());
        let mut c = Canvas::default();
        for w in &warnings {
            w.render(&mut c);
        }
        insta::assert_snapshot!(c.markup(), @r"
        <warn>warning:</> line 1: unknown key 'colour'
        <hint>hint:</> did you mean 'color'?
        <warn>warning:</> line 2: view: 'sideways' is not one of auto, split, unified
        <warn>warning:</> line 3: tab-width: '0' is not a whole number of at least 1
        <warn>warning:</> line 4: context: 'many' is not a number of lines or full
        <warn>warning:</> line 5: expected 'key = value', found 'just words'
        <hint>hint:</> comments start with '#'
        ");
    }

    /// `keybind` lines repeat; bad ones warn with a suggestion, like any
    /// other line, and leave the rest bound.
    #[test]
    fn keybind_lines_bind_keys_and_bad_ones_warn_with_hints() {
        use crate::keys::Action;
        use crate::term::{Key, Mods};
        let (config, warnings) = parse(
            "keybind = n=next_file\nkeybind = x=nxt_change\nkeybind = pagedown=top\nkeybind = tree:v=unbind\nkeybind = esc\n",
        );
        let action = |tree, c| config.keys.action(tree, Key::Char(c), Mods::NONE);
        assert_eq!(action(false, 'n'), Some(Action::NextFile));
        assert_eq!(action(false, 'x'), Some(Action::Expand));
        assert_eq!(action(true, 'v'), Some(Action::Select));
        let mut c = Canvas::default();
        for w in &warnings {
            w.render(&mut c);
        }
        insta::assert_snapshot!(c.markup(), @r"
        <warn>warning:</> line 2: keybind: unknown action 'nxt_change'
        <hint>hint:</> did you mean 'next_change'?
        <warn>warning:</> line 3: keybind: unknown key 'pagedown'
        <hint>hint:</> did you mean 'page_down'?
        <warn>warning:</> line 5: keybind: expected <key>=<action>, found 'esc'
        <hint>hint:</> for example: keybind = ctrl+d=half_page_down
        ");
        let mut c = Canvas::default();
        config.render(&Sources::default(), &mut c);
        let text = c.markup();
        assert!(
            text.ends_with(
                "<hint>keybind</><dim> = </>n=next_file\n<hint>keybind</><dim> = </>tree:v=unbind\n"
            ),
            "{text}"
        );
    }

    /// What `dffy config --default` and `dffy config` print is a config
    /// file that reads back to the same settings, bindings included.
    #[test]
    fn printed_configs_read_back_as_they_were() {
        let plain = |c: &Canvas| {
            let mut out = Vec::new();
            c.write_to(&mut out, None).expect("in memory");
            String::from_utf8(out).expect("utf-8")
        };
        let mut c = Canvas::default();
        render_defaults(&mut c);
        assert_eq!(parse(&plain(&c)), (Config::default(), Vec::new()));
        let (changed, warnings) = parse(
            "wrap = true\nkeybind = n=next_file\nkeybind = q=unbind\nkeybind = tree:x=open\n",
        );
        assert_eq!(warnings, []);
        let mut c = Canvas::default();
        let mut sources = Sources::default();
        sources.set("wrap", Source::LastUsed);
        changed.render(&sources, &mut c);
        let printed = plain(&c);
        assert!(printed.contains("wrap = true  # last used\n"), "{printed}");
        assert!(printed.contains("tab-width = 4  # default\n"), "{printed}");
        assert_eq!(parse(&printed), (changed, Vec::new()));
    }

    /// Each value also records which layer it came from.
    #[test]
    fn flags_beat_last_used_beats_config_beats_defaults() {
        let mut config = Config::default();
        let mut sources = Sources::default();
        for key in config.apply_source("wrap = true\nview = split\ntheme = mono\n", &mut Vec::new())
        {
            sources.set(key, Source::Config);
        }
        let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        let remembered = [
            pair("view", "unified"),
            pair("theme", "github-dark"),
            pair("wrap", "bogus"),
        ];
        config.layer(&mut sources, &remembered, Source::LastUsed);
        config.layer(&mut sources, &[pair("theme", "default")], Source::Flag);
        let shown = |key| (config.get(key), sources.get(key).name());
        assert_eq!(shown("theme"), ("default".to_owned(), "flag"));
        assert_eq!(shown("view"), ("unified".to_owned(), "last used"));
        assert_eq!(
            shown("wrap"),
            ("true".to_owned(), "config"),
            "a bad remembered value is skipped"
        );
        assert_eq!(shown("tab-width"), ("4".to_owned(), "default"));
    }

    #[test]
    fn defaults_listing_documents_every_key() {
        let mut c = Canvas::default();
        render_defaults(&mut c);
        let text = c.markup();
        for key in KEYS {
            assert!(
                text.contains(&format!("<hint>{}</>", key.name)),
                "{}",
                key.name
            );
        }
        insta::assert_snapshot!(text);
    }
}

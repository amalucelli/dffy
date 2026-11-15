//! Roles and themes. Everything dffy prints is tagged with a [`Role`], and
//! code text also with its [`Syntax`]; a [`Theme`] maps both to terminal
//! styles. The default theme uses only the 16 ANSI colors so the output
//! inherits the terminal's palette; truecolor is opt-in through the bundled
//! palette themes and theme files.
//!
//! When syntax colors own the foreground of code, changes still read in 16
//! colors: the sign and line numbers of changed lines carry the added or
//! removed color, and changed tokens keep their emphasis style whole (the
//! default reverses them). Under `indicators = background` changed lines are
//! tinted with hues mixed into the theme's `background` (or the terminal's,
//! for themes without one); a theme that gives `added-line` and the other
//! tinted roles a background of its own keeps it instead.

use std::fmt::Write as _;
use std::path::PathBuf;

use dffy_core::highlight::Syntax;

use crate::config::{self, Indicators, Warning};
use crate::render::Canvas;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Role {
    /// Unstyled text; not themable.
    Plain,
    Context,
    Added,
    Removed,
    AddedEmph,
    RemovedEmph,
    /// The text of an added or removed line whose code is syntax-colored.
    AddedLine,
    RemovedLine,
    LineNumber,
    /// Line numbers of added and removed lines.
    AddedNumber,
    RemovedNumber,
    /// A removed or added line paired with one on the other side as its
    /// edited version: its text (without syntax colors or tints), sign
    /// and bar.
    Modified,
    /// The text of a modified line that's syntax-colored or tinted.
    ModifiedLine,
    ModifiedNumber,
    /// A line of code the syntax diff found moved whole: its text, sign
    /// and line number.
    Moved,
    FileHeader,
    HunkHeader,
    CommitHeader,
    Error,
    Warn,
    Ok,
    Hint,
    Dim,
    /// The viewer's search matches, and the one `n`/`N` went to.
    SearchMatch,
    SearchCurrent,
    /// The viewer's status bar names what's compared in a chip of this
    /// style: `uncommitted`, `v6.0..v6.1`.
    StatusTag,
    /// The selected row of the file tree, the picker and the command
    /// panel, laid over the row's own styles: its background and
    /// attributes replace theirs, and its foreground only fills in where
    /// they have none, so status letters and counts keep their colors.
    Selection,
    /// The tree's selected row while the diff has the keys.
    SelectionInactive,
}

impl Role {
    /// Every role a theme can set, in the order `dffy config` and theme files
    /// list them.
    pub(crate) const THEMED: [Self; 27] = [
        Self::Context,
        Self::Added,
        Self::Removed,
        Self::AddedEmph,
        Self::RemovedEmph,
        Self::AddedLine,
        Self::RemovedLine,
        Self::LineNumber,
        Self::AddedNumber,
        Self::RemovedNumber,
        Self::Modified,
        Self::ModifiedLine,
        Self::ModifiedNumber,
        Self::Moved,
        Self::FileHeader,
        Self::HunkHeader,
        Self::CommitHeader,
        Self::Error,
        Self::Warn,
        Self::Ok,
        Self::Hint,
        Self::Dim,
        Self::SearchMatch,
        Self::SearchCurrent,
        Self::StatusTag,
        Self::Selection,
        Self::SelectionInactive,
    ];

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Context => "context",
            Self::Added => "added",
            Self::Removed => "removed",
            Self::AddedEmph => "added-emph",
            Self::RemovedEmph => "removed-emph",
            Self::AddedLine => "added-line",
            Self::RemovedLine => "removed-line",
            Self::LineNumber => "line-number",
            Self::AddedNumber => "added-number",
            Self::RemovedNumber => "removed-number",
            Self::Modified => "modified",
            Self::ModifiedLine => "modified-line",
            Self::ModifiedNumber => "modified-number",
            Self::Moved => "moved",
            Self::FileHeader => "file-header",
            Self::HunkHeader => "hunk-header",
            Self::CommitHeader => "commit-header",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Ok => "ok",
            Self::Hint => "hint",
            Self::Dim => "dim",
            Self::SearchMatch => "search-match",
            Self::SearchCurrent => "search-current",
            Self::StatusTag => "status-tag",
            Self::Selection => "selection",
            Self::SelectionInactive => "selection-inactive",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

const ROLE_COUNT: usize = Role::THEMED.len() + 1;
const SYNTAX_COUNT: usize = Syntax::ALL.len();

/// How a stretch of output is drawn: its role, for code its syntax, and
/// for a renderer plugin's text the attributes it asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Ink {
    pub role: Role,
    pub syntax: Option<Syntax>,
    /// Added to the style, bits as in [`Style::attributes`]. Only the
    /// viewer draws them: printed output never has any.
    pub attributes: u8,
}

impl From<Role> for Ink {
    fn from(role: Role) -> Self {
        Self {
            role,
            syntax: None,
            attributes: 0,
        }
    }
}

impl Ink {
    fn index(self) -> usize {
        self.role.index() * (SYNTAX_COUNT + 1) + self.syntax.map_or(0, |s| s as usize + 1)
    }
}

/// Every key a complete theme file sets: the background, the roles, then
/// the syntax roles. Files may also start from a built-in with `inherit`.
pub(crate) fn keys() -> Vec<String> {
    std::iter::once("background".to_owned())
        .chain(Role::THEMED.iter().map(|r| r.key().to_owned()))
        .chain(Syntax::ALL.iter().map(|s| format!("syntax-{}", s.key())))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Color {
    /// One of the 16 terminal palette colors: 0–7 normal, 8–15 bright.
    Ansi(u8),
    Rgb(u8, u8, u8),
}

const COLOR_NAMES: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

impl Color {
    fn parse(word: &str) -> Option<Self> {
        if let Some(hex) = word.strip_prefix('#') {
            if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let channel = |i| u8::from_str_radix(&hex[i..i + 2], 16).ok();
            return Some(Self::Rgb(channel(0)?, channel(2)?, channel(4)?));
        }
        let (bright, name) = match word.strip_prefix("bright-") {
            Some(name) => (8, name),
            None => (0, word),
        };
        let index = COLOR_NAMES.iter().position(|&n| n == name)?;
        Some(Self::Ansi(bright + u8::try_from(index).ok()?))
    }

    /// Appends `;` and this color's SGR parameters.
    pub(crate) fn sgr(self, background: bool, out: &mut String) {
        match self {
            Self::Ansi(n) => {
                let base = match (background, n >= 8) {
                    (false, false) => 30,
                    (false, true) => 90 - 8,
                    (true, false) => 40,
                    (true, true) => 100 - 8,
                };
                let _ = write!(out, ";{}", base + u16::from(n));
            }
            Self::Rgb(r, g, b) => {
                let _ = write!(out, ";{};2;{r};{g};{b}", if background { 48 } else { 38 });
            }
        }
    }
}

/// Text attributes as SGR codes, in emission order.
pub(crate) const ATTRIBUTES: [(&str, u8); 5] = [
    ("bold", 1),
    ("dim", 2),
    ("italic", 3),
    ("underline", 4),
    ("reverse", 7),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    /// Bit `i` set means `ATTRIBUTES[i]` is on.
    pub attributes: u8,
}

impl Style {
    pub(crate) const PLAIN: Self = Self {
        fg: None,
        bg: None,
        attributes: 0,
    };
    pub(crate) const REVERSE: u8 = 1 << 4;
    pub(crate) const UNDERLINE: u8 = 1 << 3;
    pub(crate) const ITALIC: u8 = 1 << 2;
    pub(crate) const DIM: u8 = 1 << 1;
    pub(crate) const BOLD: u8 = 1;

    /// Parses a theme value: space-separated words, where a color sets the
    /// foreground, `on <color>` sets the background, and `bold`, `dim`,
    /// `italic`, `underline` and `reverse` add attributes. An empty value is
    /// the terminal's default style.
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        let mut style = Self::default();
        let mut words = value.split_whitespace();
        while let Some(word) = words.next() {
            if word == "on" {
                let color = words.next().ok_or("`on` needs a color after it")?;
                style.bg = Some(Color::parse(color).ok_or_else(|| bad_color(color))?);
            } else if let Some(i) = ATTRIBUTES.iter().position(|(name, _)| *name == word) {
                style.attributes |= 1 << i;
            } else {
                style.fg = Some(Color::parse(word).ok_or_else(|| bad_color(word))?);
            }
        }
        Ok(style)
    }

    /// The escape sequence that switches from the default style to this one;
    /// empty for the default style.
    fn sgr(&self) -> String {
        if *self == Self::default() {
            return String::new();
        }
        let mut out = String::from("\x1b[0");
        for (i, (_, code)) in ATTRIBUTES.iter().enumerate() {
            if self.attributes & (1 << i) != 0 {
                let _ = write!(out, ";{code}");
            }
        }
        if let Some(fg) = self.fg {
            fg.sgr(false, &mut out);
        }
        if let Some(bg) = self.bg {
            bg.sgr(true, &mut out);
        }
        out.push('m');
        out
    }
}

fn bad_color(word: &str) -> String {
    format!(
        "'{word}' is not a color or attribute; use a name like 'green' or 'bright-green', '#rrggbb', \
         or bold/dim/italic/underline/reverse"
    )
}

/// The style of every role and syntax role, with the escape sequence of
/// every combination precomputed.
#[derive(Debug, Clone)]
pub(crate) struct Theme {
    pub name: String,
    styles: [Style; ROLE_COUNT],
    syntax: [Style; SYNTAX_COUNT],
    /// By [`Ink::index`].
    sgr: Vec<String>,
    /// Bit `i` set: [`TINTS`] entry `i` set its role's background.
    tinted: u16,
    /// The background the theme's colors were picked for. dffy doesn't
    /// paint it; tints mix into it, and it sorts the theme dark or light.
    background: Option<Rgb>,
}

pub(crate) type Rgb = (u8, u8, u8);

/// The terminal backgrounds assumed when it doesn't say.
pub(crate) const DARK_BACKGROUND: Rgb = (0x1e, 0x1e, 0x2e);
pub(crate) const LIGHT_BACKGROUND: Rgb = (0xff, 0xff, 0xff);

/// The hues of `indicators = background` as OKLCH hue angles: green for
/// added, red for removed, and purple for moved code so it never reads as
/// either. They share a lightness and chroma, so their tints come out
/// equally strong.
const ADDED_HUE: f64 = 145.0;
const REMOVED_HUE: f64 = 25.0;
const MOVED_HUE: f64 = 305.0;
/// Modified lines take the hue of the theme's `modified` color, its own
/// blue, when it falls in this band of blues and indigos, clear of the
/// added, removed and moved hues; otherwise (a palette color, whose value
/// isn't known, or a theme whose blue is a teal or a purple) this one.
const MODIFIED_HUES: std::ops::RangeInclusive<f64> = 225.0..=280.0;
const MODIFIED_HUE: f64 = 260.0;
const HUE_LIGHTNESS: f64 = 0.66;
const HUE_CHROMA: f64 = 0.17;

/// How much of the hue each tint mixes into the background: the line, its
/// number cell (stronger, so the gutter reads at a glance), and changed
/// tokens (strongest).
const LINE_TINT: f64 = 0.15;
const GUTTER_TINT: f64 = 0.25;
const EMPH_TINT: f64 = 0.35;

/// The tints of `indicators = background`: each role's hue (`None` for the
/// theme's modified hue) and share; reformatted code (dim) gets none.
const TINTS: [(Role, Option<f64>, f64); 9] = [
    (Role::AddedLine, Some(ADDED_HUE), LINE_TINT),
    (Role::AddedNumber, Some(ADDED_HUE), GUTTER_TINT),
    (Role::AddedEmph, Some(ADDED_HUE), EMPH_TINT),
    (Role::RemovedLine, Some(REMOVED_HUE), LINE_TINT),
    (Role::RemovedNumber, Some(REMOVED_HUE), GUTTER_TINT),
    (Role::RemovedEmph, Some(REMOVED_HUE), EMPH_TINT),
    (Role::ModifiedLine, None, LINE_TINT),
    (Role::ModifiedNumber, None, GUTTER_TINT),
    (Role::Moved, Some(MOVED_HUE), LINE_TINT),
];

/// Color math for the tints: sRGB to Oklab and back, where mixing keeps
/// perceived lightness even, and WCAG contrast for readable numbers.
pub(crate) mod color {
    use super::Rgb;

    fn linear(c: u8) -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    fn encode(c: f64) -> u8 {
        let c = c.clamp(0.0, 1.0);
        let c = if c <= 0.003_130_8 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        };
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to 0..=255"
        )]
        let byte = (c * 255.0).round() as u8;
        byte
    }

    type Matrix = [[f64; 3]; 3];

    /// Björn Ottosson's Oklab matrices: linear sRGB to cone responses,
    /// cone responses to Oklab, and the inverses.
    const TO_LMS: Matrix = [
        [0.412_221_470_8, 0.536_332_536_3, 0.051_445_992_9],
        [0.211_903_498_2, 0.680_699_545_1, 0.107_396_956_6],
        [0.088_302_461_9, 0.281_718_837_6, 0.629_978_700_5],
    ];
    const TO_LAB: Matrix = [
        [0.210_454_255_3, 0.793_617_785, -0.004_072_046_8],
        [1.977_998_495_1, -2.428_592_205, 0.450_593_709_9],
        [0.025_904_037_1, 0.782_771_766_2, -0.808_675_766],
    ];
    const FROM_LAB: Matrix = [
        [1.0, 0.396_337_777_4, 0.215_803_757_3],
        [1.0, -0.105_561_345_8, -0.063_854_172_8],
        [1.0, -0.089_484_177_5, -1.291_485_548],
    ];
    const FROM_LMS: Matrix = [
        [4.076_741_662_1, -3.307_711_591_3, 0.230_969_929_2],
        [-1.268_438_004_6, 2.609_757_401_1, -0.341_319_396_5],
        [-0.004_196_086_3, -0.703_418_614_7, 1.707_614_701],
    ];

    fn apply(m: &Matrix, v: [f64; 3]) -> [f64; 3] {
        m.map(|row| row[0] * v[0] + row[1] * v[1] + row[2] * v[2])
    }

    pub(crate) fn to_oklab((r, g, b): Rgb) -> [f64; 3] {
        let lms = apply(&TO_LMS, [linear(r), linear(g), linear(b)]);
        apply(&TO_LAB, lms.map(f64::cbrt))
    }

    pub(crate) fn from_oklab(lab: [f64; 3]) -> Rgb {
        let lms = apply(&FROM_LAB, lab).map(|v| v.powi(3));
        let [r, g, b] = apply(&FROM_LMS, lms).map(encode);
        (r, g, b)
    }

    /// The Oklch hue of `rgb`, in degrees.
    pub(crate) fn hue(rgb: Rgb) -> f64 {
        let [_, a, b] = to_oklab(rgb);
        b.atan2(a).to_degrees().rem_euclid(360.0)
    }

    /// The color at Oklch `lightness`, `chroma` and `hue` degrees.
    pub(crate) fn oklch(lightness: f64, chroma: f64, hue: f64) -> Rgb {
        let hue = hue.to_radians();
        from_oklab([lightness, chroma * hue.cos(), chroma * hue.sin()])
    }

    /// `share` of `top` over `base`, mixed in Oklab.
    pub(crate) fn blend(base: Rgb, top: Rgb, share: f64) -> Rgb {
        let (a, b) = (to_oklab(base), to_oklab(top));
        from_oklab([0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * share))
    }

    /// WCAG relative luminance.
    pub(crate) fn luminance((r, g, b): Rgb) -> f64 {
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    }

    /// WCAG contrast ratio, 1 to 21.
    pub(crate) fn contrast(x: Rgb, y: Rgb) -> f64 {
        let (a, b) = (luminance(x), luminance(y));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    pub(crate) fn is_light(rgb: Rgb) -> bool {
        luminance(rgb) > 0.18
    }

    /// Text in hue `h` that reads on `bg` at a contrast of 4.5 or more: as
    /// colorful as it can be, made lighter (on dark) or darker until it
    /// does.
    pub(crate) fn readable(h: f64, bg: Rgb) -> Rgb {
        let light = is_light(bg);
        let mut l = if light { 0.45 } else { 0.8 };
        loop {
            let fg = oklch(l, 0.12, h);
            if contrast(fg, bg) >= 4.5 || !(0.0..=1.0).contains(&l) {
                return fg;
            }
            l += if light { -0.03 } else { 0.03 };
        }
    }
}

/// The themes that follow the terminal's palette; [`PALETTES`] has the
/// truecolor ones.
pub(crate) const BUILTIN: [(&str, &str); 2] = [
    (
        "default",
        "# 16 ANSI colors, so dffy follows the terminal's palette.
background =
context =
added = green
removed = red
added-emph = green reverse
removed-emph = red reverse
added-line =
removed-line =
line-number = dim
added-number = green
removed-number = red
modified = blue
modified-line =
modified-number = blue
moved = magenta
file-header = bold blue
hunk-header = cyan
commit-header = yellow
error = bold red
warn = yellow
ok = green
hint = cyan
dim = dim
search-match = black on yellow
search-current = bold black on bright-cyan
status-tag = black on blue
selection = on bright-black
selection-inactive = on bright-black
syntax-keyword = magenta
syntax-string = green
syntax-escape = cyan
syntax-comment = dim
syntax-function = blue
syntax-type = yellow
syntax-number = cyan
syntax-constant = cyan
syntax-operator =
syntax-punctuation =
syntax-property =
syntax-variable-builtin = magenta
syntax-attribute = yellow
syntax-tag = blue
syntax-label = cyan",
    ),
    (
        "mono",
        "# No colors, only attributes.
background =
context =
added = bold
removed = dim
added-emph = bold reverse
removed-emph = reverse
added-line =
removed-line =
line-number = dim
added-number = bold
removed-number = dim
modified = underline
modified-line =
modified-number = underline
moved = italic
file-header = bold underline
hunk-header = italic
commit-header = bold
error = bold
warn = bold
ok =
hint = italic
dim = dim
search-match = underline
search-current = reverse
status-tag = reverse
selection = reverse
selection-inactive = reverse
syntax-keyword = bold
syntax-string =
syntax-escape =
syntax-comment = italic
syntax-function =
syntax-type =
syntax-number =
syntax-constant =
syntax-operator =
syntax-punctuation =
syntax-property =
syntax-variable-builtin =
syntax-attribute =
syntax-tag =
syntax-label =",
    ),
];

/// A bundled truecolor theme: colors from its published palette, laid onto
/// the roles the same way for every theme by [`Palette::source`], and its
/// own syntax colors. Emphasis is reversed and line roles set no
/// background, so `indicators = background` derives every tint from `bg`.
struct Palette {
    name: &'static str,
    bg: &'static str,
    fg: &'static str,
    /// Line numbers.
    gutter: &'static str,
    /// Dim text, borders and separators.
    muted: &'static str,
    green: &'static str,
    red: &'static str,
    /// Modified files, warnings, commit ids and search matches.
    yellow: &'static str,
    /// File names and the status bar's chip.
    blue: &'static str,
    /// Hunk headers, hints, and renamed files.
    cyan: &'static str,
    /// Moved code.
    purple: &'static str,
    /// The current search match.
    orange: &'static str,
    /// Every `syntax-` line.
    syntax: &'static str,
}

impl Palette {
    fn source(&self) -> String {
        let Self {
            bg,
            fg,
            gutter,
            muted,
            green,
            red,
            yellow,
            blue,
            cyan,
            purple,
            orange,
            syntax,
            ..
        } = self;
        let rgb = |hex: &str| match Color::parse(hex) {
            Some(Color::Rgb(r, g, b)) => (r, g, b),
            _ => (0, 0, 0),
        };
        let hex = |(r, g, b): Rgb| format!("#{r:02x}{g:02x}{b:02x}");
        let lift = |share| hex(color::blend(rgb(bg), rgb(fg), share));
        let (selection, inactive) = (lift(SELECTION_LIFT), lift(SELECTION_LIFT / 2.0));
        // Background-colored text on the accent, or where that reads worse
        // (the pale accents of light palettes), the text color on the
        // accent mixed into the background.
        let chip = |accent: &str| {
            let mixed = color::blend(rgb(bg), rgb(accent), CHIP_MIX);
            if color::contrast(rgb(bg), rgb(accent)) >= color::contrast(rgb(fg), mixed) {
                format!("{bg} on {accent}")
            } else {
                format!("{fg} on {}", hex(mixed))
            }
        };
        let (search, current, tag) = (chip(yellow), chip(orange), chip(blue));
        // Modified lines take the blue, unless it's a purple or a teal
        // (Dracula, Gruvbox, Nord) that moved or added code would share:
        // then a blue as light and colorful as it.
        let modified = {
            let blue = rgb(blue);
            if MODIFIED_HUES.contains(&color::hue(blue)) {
                hex(blue)
            } else {
                let [l, a, b] = color::to_oklab(blue);
                hex(color::oklch(l, a.hypot(b), MODIFIED_HUE))
            }
        };
        format!(
            "background = {bg}
context = {fg}
added = {green}
removed = {red}
added-emph = {green} reverse
removed-emph = {red} reverse
added-line = {fg}
removed-line = {fg}
line-number = {gutter}
added-number = {green}
removed-number = {red}
modified = {modified}
modified-line = {fg}
modified-number = {modified}
moved = {purple}
file-header = bold {blue}
hunk-header = {cyan}
commit-header = {yellow}
error = bold {red}
warn = {yellow}
ok = {green}
hint = {cyan}
dim = {muted}
search-match = {search}
search-current = bold {current}
status-tag = {tag}
selection = on {selection}
selection-inactive = on {inactive}
{syntax}"
        )
    }
}

/// How far a bundled theme's selected row moves from its background
/// toward its text color, in Oklab: enough to find the row, little enough
/// that the colored text on it still reads. The inactive selection moves
/// half as far.
const SELECTION_LIFT: f64 = 0.08;

/// How much of an accent a chip mixes into the background when the theme's
/// text goes on it.
const CHIP_MIX: f64 = 0.25;

/// Dark themes first, then light ones, the order `dffy themes` groups them
/// in and the command panel cycles through them.
const PALETTES: [Palette; 14] = [
    // Primer primitives v7 through primer/github-vscode-theme (GitHub Dark
    // Default): https://github.com/primer/github-vscode-theme
    Palette {
        name: "github-dark",
        bg: "#0d1117",
        fg: "#c9d1d9",
        gutter: "#6e7681",
        muted: "#8b949e",
        green: "#3fb950",
        red: "#f85149",
        yellow: "#d29922",
        blue: "#58a6ff",
        cyan: "#79c0ff",
        purple: "#d2a8ff",
        orange: "#ffa657",
        syntax: "syntax-keyword = #ff7b72
syntax-string = #a5d6ff
syntax-escape = #ff7b72
syntax-comment = #8b949e
syntax-function = #d2a8ff
syntax-type = #ffa657
syntax-number = #79c0ff
syntax-constant = #79c0ff
syntax-operator = #ff7b72
syntax-punctuation =
syntax-property = #79c0ff
syntax-variable-builtin = #79c0ff
syntax-attribute = #79c0ff
syntax-tag = #7ee787
syntax-label = #ffa657",
    },
    // folke/tokyonight.nvim, the night style (storm's palette on a darker
    // background): https://github.com/folke/tokyonight.nvim
    Palette {
        name: "tokyo-night",
        bg: "#1a1b26",
        fg: "#c0caf5",
        gutter: "#737aa2",
        muted: "#565f89",
        green: "#9ece6a",
        red: "#f7768e",
        yellow: "#e0af68",
        blue: "#7aa2f7",
        cyan: "#7dcfff",
        purple: "#bb9af7",
        orange: "#ff9e64",
        syntax: "syntax-keyword = #bb9af7
syntax-string = #9ece6a
syntax-escape = #bb9af7
syntax-comment = italic #565f89
syntax-function = #7aa2f7
syntax-type = #2ac3de
syntax-number = #ff9e64
syntax-constant = #ff9e64
syntax-operator = #89ddff
syntax-punctuation = #89ddff
syntax-property = #73daca
syntax-variable-builtin = #f7768e
syntax-attribute = #bb9af7
syntax-tag = #f7768e
syntax-label = #7aa2f7",
    },
    // Catppuccin Mocha, from catppuccin/palette and its style guide:
    // https://github.com/catppuccin/palette/blob/main/palette.json
    Palette {
        name: "catppuccin-mocha",
        bg: "#1e1e2e",
        fg: "#cdd6f4",
        gutter: "#7f849c",
        muted: "#6c7086",
        green: "#a6e3a1",
        red: "#f38ba8",
        yellow: "#f9e2af",
        blue: "#89b4fa",
        cyan: "#89dceb",
        purple: "#cba6f7",
        orange: "#fab387",
        syntax: "syntax-keyword = #cba6f7
syntax-string = #a6e3a1
syntax-escape = #f5c2e7
syntax-comment = italic #9399b2
syntax-function = #89b4fa
syntax-type = #f9e2af
syntax-number = #fab387
syntax-constant = #fab387
syntax-operator = #89dceb
syntax-punctuation = #9399b2
syntax-property = #b4befe
syntax-variable-builtin = #f38ba8
syntax-attribute = #f9e2af
syntax-tag = #89b4fa
syntax-label = #74c7ec",
    },
    // morhetz/gruvbox, bright colors on dark0:
    // https://github.com/morhetz/gruvbox
    Palette {
        name: "gruvbox-dark",
        bg: "#282828",
        fg: "#ebdbb2",
        gutter: "#7c6f64",
        muted: "#928374",
        green: "#b8bb26",
        red: "#fb4934",
        yellow: "#fabd2f",
        blue: "#83a598",
        cyan: "#8ec07c",
        purple: "#d3869b",
        orange: "#fe8019",
        syntax: "syntax-keyword = #fb4934
syntax-string = #b8bb26
syntax-escape = #fe8019
syntax-comment = italic #928374
syntax-function = bold #b8bb26
syntax-type = #fabd2f
syntax-number = #d3869b
syntax-constant = #d3869b
syntax-operator =
syntax-punctuation =
syntax-property = #83a598
syntax-variable-builtin = #fe8019
syntax-attribute = #8ec07c
syntax-tag = #83a598
syntax-label = #fb4934",
    },
    // Nord: https://www.nordtheme.com/docs/colors-and-palettes. Comments
    // and line numbers take #616e88, the brightened nord3 its editor ports
    // use, since nord3 itself is too dim to read as text.
    Palette {
        name: "nord",
        bg: "#2e3440",
        fg: "#d8dee9",
        gutter: "#616e88",
        muted: "#616e88",
        green: "#a3be8c",
        red: "#bf616a",
        yellow: "#ebcb8b",
        blue: "#88c0d0",
        cyan: "#8fbcbb",
        purple: "#b48ead",
        orange: "#d08770",
        syntax: "syntax-keyword = #81a1c1
syntax-string = #a3be8c
syntax-escape = #ebcb8b
syntax-comment = #616e88
syntax-function = #88c0d0
syntax-type = #8fbcbb
syntax-number = #b48ead
syntax-constant = #d8dee9
syntax-operator = #81a1c1
syntax-punctuation = #eceff4
syntax-property = #d8dee9
syntax-variable-builtin = #81a1c1
syntax-attribute = #d08770
syntax-tag = #81a1c1
syntax-label = #8fbcbb",
    },
    // Dracula's specification: https://spec.draculatheme.com
    Palette {
        name: "dracula",
        bg: "#282a36",
        fg: "#f8f8f2",
        gutter: "#6272a4",
        muted: "#6272a4",
        green: "#50fa7b",
        red: "#ff5555",
        yellow: "#f1fa8c",
        blue: "#bd93f9",
        cyan: "#8be9fd",
        purple: "#bd93f9",
        orange: "#ffb86c",
        syntax: "syntax-keyword = #ff79c6
syntax-string = #f1fa8c
syntax-escape = #ff79c6
syntax-comment = #6272a4
syntax-function = #50fa7b
syntax-type = italic #8be9fd
syntax-number = #bd93f9
syntax-constant = #bd93f9
syntax-operator = #ff79c6
syntax-punctuation =
syntax-property =
syntax-variable-builtin = italic #bd93f9
syntax-attribute = italic #50fa7b
syntax-tag = #ff79c6
syntax-label = #8be9fd",
    },
    // Atom's One Dark as joshdick/onedark.vim maps it:
    // https://github.com/joshdick/onedark.vim
    Palette {
        name: "one-dark",
        bg: "#282c34",
        fg: "#abb2bf",
        gutter: "#5c6370",
        muted: "#5c6370",
        green: "#98c379",
        red: "#e06c75",
        yellow: "#e5c07b",
        blue: "#61afef",
        cyan: "#56b6c2",
        purple: "#c678dd",
        orange: "#d19a66",
        syntax: "syntax-keyword = #c678dd
syntax-string = #98c379
syntax-escape = #56b6c2
syntax-comment = #5c6370
syntax-function = #61afef
syntax-type = #e5c07b
syntax-number = #d19a66
syntax-constant = #d19a66
syntax-operator = #c678dd
syntax-punctuation =
syntax-property = #e06c75
syntax-variable-builtin = #e5c07b
syntax-attribute = #d19a66
syntax-tag = #e06c75
syntax-label = #c678dd",
    },
    // Solarized, dark mode (base03 background, base0 text):
    // https://ethanschoonover.com/solarized
    Palette {
        name: "solarized-dark",
        bg: "#002b36",
        fg: "#839496",
        gutter: "#586e75",
        muted: "#586e75",
        green: "#859900",
        red: "#dc322f",
        yellow: "#b58900",
        blue: "#268bd2",
        cyan: "#2aa198",
        purple: "#6c71c4",
        orange: "#cb4b16",
        syntax: "syntax-keyword = #859900
syntax-string = #2aa198
syntax-escape = #dc322f
syntax-comment = italic #586e75
syntax-function = #268bd2
syntax-type = #b58900
syntax-number = #2aa198
syntax-constant = #2aa198
syntax-operator = #859900
syntax-punctuation =
syntax-property = #268bd2
syntax-variable-builtin = #cb4b16
syntax-attribute = #cb4b16
syntax-tag = #268bd2
syntax-label = #859900",
    },
    // Rosé Pine, the main variant, as rose-pine/neovim defines and maps it:
    // https://github.com/rose-pine/neovim/blob/main/lua/rose-pine/palette.lua.
    // It has no green; additions are foam, as in its own git colors.
    Palette {
        name: "rose-pine",
        bg: "#191724",
        fg: "#e0def4",
        gutter: "#6e6a86",
        muted: "#6e6a86",
        green: "#9ccfd8",
        red: "#eb6f92",
        yellow: "#f6c177",
        blue: "#c4a7e7",
        cyan: "#ebbcba",
        purple: "#c4a7e7",
        orange: "#ebbcba",
        syntax: "syntax-keyword = #31748f
syntax-string = #f6c177
syntax-escape = #31748f
syntax-comment = italic #908caa
syntax-function = #ebbcba
syntax-type = #9ccfd8
syntax-number = #f6c177
syntax-constant = #f6c177
syntax-operator = #908caa
syntax-punctuation = #908caa
syntax-property = italic #9ccfd8
syntax-variable-builtin = bold italic #eb6f92
syntax-attribute = #c4a7e7
syntax-tag = #9ccfd8
syntax-label = #9ccfd8",
    },
    // rebelot/kanagawa.nvim, the wave theme:
    // https://github.com/rebelot/kanagawa.nvim
    Palette {
        name: "kanagawa",
        bg: "#1f1f28",
        fg: "#dcd7ba",
        gutter: "#54546d",
        muted: "#727169",
        green: "#76946a",
        red: "#c34043",
        yellow: "#dca561",
        blue: "#7e9cd8",
        cyan: "#7fb4ca",
        purple: "#957fb8",
        orange: "#ffa066",
        syntax: "syntax-keyword = #957fb8
syntax-string = #98bb6c
syntax-escape = #c0a36e
syntax-comment = #727169
syntax-function = #7e9cd8
syntax-type = #7aa89f
syntax-number = #d27e99
syntax-constant = #ffa066
syntax-operator = #c0a36e
syntax-punctuation = #9cabca
syntax-property = #e6c384
syntax-variable-builtin = #ff5d62
syntax-attribute = #e6c384
syntax-tag = #e46876
syntax-label = #7fb4ca",
    },
    // Primer primitives v7 through primer/github-vscode-theme (GitHub Light
    // Default): https://github.com/primer/github-vscode-theme
    Palette {
        name: "github-light",
        bg: "#ffffff",
        fg: "#24292f",
        gutter: "#8c959f",
        muted: "#6e7781",
        green: "#1a7f37",
        red: "#cf222e",
        yellow: "#9a6700",
        blue: "#0969da",
        cyan: "#0550ae",
        purple: "#8250df",
        orange: "#bc4c00",
        syntax: "syntax-keyword = #cf222e
syntax-string = #0a3069
syntax-escape = #cf222e
syntax-comment = #6e7781
syntax-function = #8250df
syntax-type = #953800
syntax-number = #0550ae
syntax-constant = #0550ae
syntax-operator = #cf222e
syntax-punctuation =
syntax-property = #0550ae
syntax-variable-builtin = #0550ae
syntax-attribute = #0550ae
syntax-tag = #116329
syntax-label = #953800",
    },
    // Catppuccin Latte, from catppuccin/palette and its style guide:
    // https://github.com/catppuccin/palette/blob/main/palette.json
    Palette {
        name: "catppuccin-latte",
        bg: "#eff1f5",
        fg: "#4c4f69",
        gutter: "#8c8fa1",
        muted: "#9ca0b0",
        green: "#40a02b",
        red: "#d20f39",
        yellow: "#df8e1d",
        blue: "#1e66f5",
        cyan: "#04a5e5",
        purple: "#8839ef",
        orange: "#fe640b",
        syntax: "syntax-keyword = #8839ef
syntax-string = #40a02b
syntax-escape = #ea76cb
syntax-comment = italic #7c7f93
syntax-function = #1e66f5
syntax-type = #df8e1d
syntax-number = #fe640b
syntax-constant = #fe640b
syntax-operator = #04a5e5
syntax-punctuation = #7c7f93
syntax-property = #7287fd
syntax-variable-builtin = #d20f39
syntax-attribute = #df8e1d
syntax-tag = #1e66f5
syntax-label = #209fb5",
    },
    // morhetz/gruvbox, faded colors on light0:
    // https://github.com/morhetz/gruvbox
    Palette {
        name: "gruvbox-light",
        bg: "#fbf1c7",
        fg: "#3c3836",
        gutter: "#a89984",
        muted: "#928374",
        green: "#79740e",
        red: "#9d0006",
        yellow: "#b57614",
        blue: "#076678",
        cyan: "#427b58",
        purple: "#8f3f71",
        orange: "#af3a03",
        syntax: "syntax-keyword = #9d0006
syntax-string = #79740e
syntax-escape = #af3a03
syntax-comment = italic #928374
syntax-function = bold #79740e
syntax-type = #b57614
syntax-number = #8f3f71
syntax-constant = #8f3f71
syntax-operator =
syntax-punctuation =
syntax-property = #076678
syntax-variable-builtin = #af3a03
syntax-attribute = #427b58
syntax-tag = #076678
syntax-label = #9d0006",
    },
    // Solarized, light mode (base3 background, base00 text):
    // https://ethanschoonover.com/solarized
    Palette {
        name: "solarized-light",
        bg: "#fdf6e3",
        fg: "#657b83",
        gutter: "#93a1a1",
        muted: "#93a1a1",
        green: "#859900",
        red: "#dc322f",
        yellow: "#b58900",
        blue: "#268bd2",
        cyan: "#2aa198",
        purple: "#6c71c4",
        orange: "#cb4b16",
        syntax: "syntax-keyword = #859900
syntax-string = #2aa198
syntax-escape = #dc322f
syntax-comment = italic #93a1a1
syntax-function = #268bd2
syntax-type = #b58900
syntax-number = #2aa198
syntax-constant = #2aa198
syntax-operator = #859900
syntax-punctuation =
syntax-property = #268bd2
syntax-variable-builtin = #cb4b16
syntax-attribute = #cb4b16
syntax-tag = #268bd2
syntax-label = #859900",
    },
];

/// Every built-in theme's name: the terminal-palette ones, then the
/// truecolor ones dark to light.
pub(crate) fn builtin_names() -> impl Iterator<Item = &'static str> {
    BUILTIN
        .iter()
        .map(|(name, _)| *name)
        .chain(PALETTES.iter().map(|p| p.name))
}

/// A built-in theme as theme file text, every key set.
fn builtin_source(name: &str) -> Option<String> {
    if let Some((_, source)) = BUILTIN.iter().find(|(n, _)| *n == name) {
        return Some((*source).to_owned());
    }
    PALETTES
        .iter()
        .find(|p| p.name == name)
        .map(Palette::source)
}

fn unknown_builtin(name: &str) -> String {
    let mut message = format!("no built-in theme '{name}'");
    if let Some(close) = dffy_core::suggest::closest(name, builtin_names()) {
        let _ = write!(message, "; did you mean '{close}'?");
    }
    message
}

/// `dffy themes --dump`: a built-in theme as a complete file to copy into
/// the themes directory and edit. User files are never read, so a file
/// named like the built-in doesn't stand in for it.
pub(crate) fn dump(name: &str) -> Result<String, String> {
    let source = builtin_source(name).ok_or_else(|| unknown_builtin(name))?;
    Ok(format!(
        "# The built-in '{name}' theme, every key set. Saved in the themes\n\
         # directory under a name of your own, it's a theme of that name.\n\
         # To change a few colors only, keep those lines below `inherit = {name}`.\n\
         {source}\n"
    ))
}

impl Theme {
    pub(crate) fn default_theme() -> Self {
        let empty = Self {
            name: String::new(),
            styles: [Style::default(); ROLE_COUNT],
            syntax: [Style::default(); SYNTAX_COUNT],
            sgr: Vec::new(),
            tinted: 0,
            background: None,
        };
        empty.parse_onto("default", BUILTIN[0].1, &mut Vec::new())
    }

    /// Builds a theme from `key = value` lines. Keys the source doesn't set
    /// keep the style of the built-in its `inherit` line names, wherever
    /// that line is, or else the default theme's; bad lines become
    /// warnings.
    pub(crate) fn parse(name: &str, source: &str, warnings: &mut Vec<Warning>) -> Self {
        let base = Self::default_theme();
        let base = match inherited(source).and_then(builtin_source) {
            Some(parent) => base.parse_onto(name, &parent, &mut Vec::new()),
            None => base,
        };
        base.parse_onto(name, source, warnings)
    }

    fn parse_onto(mut self, name: &str, source: &str, warnings: &mut Vec<Warning>) -> Self {
        name.clone_into(&mut self.name);
        let keys = keys();
        let keys: Vec<&str> = std::iter::once("inherit")
            .chain(keys.iter().map(String::as_str))
            .collect();
        let roles = 2..2 + Role::THEMED.len();
        config::parse_lines(source, &keys, warnings, |key, value| {
            match key {
                // Applied before any line, by `parse`.
                0 => {
                    if builtin_source(value).is_none() {
                        return Err(unknown_builtin(value).into());
                    }
                }
                1 => {
                    self.background = match Color::parse(value) {
                        _ if value.is_empty() => None,
                        Some(Color::Rgb(r, g, b)) => Some((r, g, b)),
                        _ => return Err(format!("'{value}' is not a #rrggbb color").into()),
                    };
                }
                _ if roles.contains(&key) => {
                    self.styles[Role::THEMED[key - roles.start].index()] = Style::parse(value)?;
                }
                _ => self.syntax[key - roles.end] = Style::parse(value)?,
            }
            Ok(())
        });
        self.cache_sgr();
        self
    }

    fn cache_sgr(&mut self) {
        let mut sgr = vec![String::new(); ROLE_COUNT * (SYNTAX_COUNT + 1)];
        for role in std::iter::once(Role::Plain).chain(Role::THEMED) {
            for syntax in std::iter::once(None).chain(Syntax::ALL.map(Some)) {
                let ink = Ink {
                    role,
                    syntax,
                    attributes: 0,
                };
                sgr[ink.index()] = self.ink_style(ink).sgr();
            }
        }
        self.sgr = sgr;
    }

    /// Finds `name` among the user's theme files, then the built-ins. A user
    /// file named like a built-in replaces it.
    pub(crate) fn load(name: &str, warnings: &mut Vec<Warning>) -> Result<Self, String> {
        if let Some(dir) = themes_dir() {
            let path = dir.join(name);
            if let Ok(source) = std::fs::read_to_string(&path) {
                let mut file_warnings = Vec::new();
                let theme = Self::parse(name, &source, &mut file_warnings);
                warnings.extend(file_warnings.into_iter().map(|w| w.in_file(&path)));
                return Ok(theme);
            }
        }
        if let Some(source) = builtin_source(name) {
            return Ok(Self::parse(name, &source, warnings));
        }
        let names = available();
        let mut message = format!("unknown theme '{name}'");
        if let Some(close) =
            dffy_core::suggest::closest(name, names.iter().map(|(n, _)| n.as_str()))
        {
            let _ = write!(message, "; did you mean '{close}'?");
        }
        Err(message)
    }

    /// Lays the background tints of `indicators = background` over the
    /// roles of changed lines, each its hue mixed at its share into the
    /// theme's background, or the terminal's background `bg` when the
    /// theme names none. A role the theme already gives a background keeps
    /// it; reversed emphasis turns into a stronger tint under the text
    /// color, so syntax colors stay on top; line numbers get a color that
    /// reads on their tint. Calling it again replaces the tints it laid.
    pub(crate) fn tint(&mut self, bg: Rgb) {
        let bg = self.background.unwrap_or(bg);
        let text = self.styles[Role::Context.index()].fg;
        let modified = self.modified_hue();
        for (i, &(role, hue, share)) in TINTS.iter().enumerate() {
            let hue = hue.unwrap_or(modified);
            let style = &mut self.styles[role.index()];
            let ours = self.tinted & (1 << i) != 0;
            if style.bg.is_some() && !ours {
                continue;
            }
            if style.attributes & Style::REVERSE != 0 {
                style.attributes &= !Style::REVERSE;
                style.fg = text;
            }
            let tint = color::blend(bg, color::oklch(HUE_LIGHTNESS, HUE_CHROMA, hue), share);
            style.bg = Some(Color::Rgb(tint.0, tint.1, tint.2));
            if matches!(
                role,
                Role::AddedNumber | Role::RemovedNumber | Role::ModifiedNumber
            ) {
                let reads = matches!(style.fg, Some(Color::Rgb(r, g, b))
                    if color::contrast((r, g, b), tint) >= 4.5);
                if !reads {
                    let (r, g, b) = color::readable(hue, tint);
                    style.fg = Some(Color::Rgb(r, g, b));
                }
            }
            self.tinted |= 1 << i;
        }
        self.cache_sgr();
    }

    /// The hue modified lines are tinted with; see [`MODIFIED_HUES`].
    fn modified_hue(&self) -> f64 {
        match self.styles[Role::Modified.index()].fg {
            Some(Color::Rgb(r, g, b)) => Some(color::hue((r, g, b))),
            _ => None,
        }
        .filter(|hue| MODIFIED_HUES.contains(hue))
        .unwrap_or(MODIFIED_HUE)
    }

    /// The theme's index in [`GROUPS`]: the terminal's palette for a theme
    /// without a background, else dark or light by it.
    pub(crate) fn group(&self) -> usize {
        match self.background {
            None => 0,
            Some(bg) if color::is_light(bg) => 2,
            Some(_) => 1,
        }
    }

    /// Gives every style without a background the theme's own, so a sample
    /// reads as it would on a terminal set to it, whatever this one is.
    fn on_own_background(&mut self) {
        let Some((r, g, b)) = self.background else {
            return;
        };
        for style in &mut self.styles {
            style.bg = style.bg.or(Some(Color::Rgb(r, g, b)));
        }
        self.cache_sgr();
    }

    pub(crate) fn style(&self, role: Role) -> Style {
        self.styles[role.index()]
    }

    /// `style` on a selected row: `selection` (or `selection-inactive`
    /// when the list doesn't have the keys) laid over it, its background
    /// and attributes over the style's, its foreground under the style's.
    pub(crate) fn selected(&self, style: Style, focused: bool) -> Style {
        let selection = self.style(if focused {
            Role::Selection
        } else {
            Role::SelectionInactive
        });
        Style {
            fg: style.fg.or(selection.fg),
            bg: selection.bg.or(style.bg),
            attributes: style.attributes | selection.attributes,
        }
    }

    /// The role's style with the syntax role's laid over it: the syntax
    /// sets the foreground and adds attributes, the role keeps its
    /// background. A reversed role keeps its own style whole, since the
    /// syntax foreground would become its background. The ink's own
    /// attributes go on top.
    pub(crate) fn ink_style(&self, ink: Ink) -> Style {
        let base = self.style(ink.role);
        let mut style = match ink.syntax {
            Some(syntax) if base.attributes & Style::REVERSE == 0 => {
                let over = self.syntax[syntax as usize];
                Style {
                    fg: over.fg.or(base.fg),
                    bg: base.bg,
                    attributes: base.attributes | over.attributes,
                }
            }
            _ => base,
        };
        style.attributes |= ink.attributes;
        style
    }

    /// The escape sequence of `ink`, without its own attributes (see
    /// [`Ink::attributes`]).
    pub(crate) fn sgr(&self, ink: Ink) -> &str {
        &self.sgr[ink.index()]
    }
}

pub(crate) fn themes_dir() -> Option<PathBuf> {
    config::config_dir().map(|d| d.join("themes"))
}

/// The value of the source's `inherit` line, if it has one.
fn inherited(source: &str) -> Option<&str> {
    source.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "inherit").then(|| value.trim().trim_matches('"'))
    })
}

/// Built-in theme names, then user theme files in name order, each with the
/// file it comes from (`None` for built-ins). A file named like a built-in
/// takes its place.
pub(crate) fn available() -> Vec<(String, Option<PathBuf>)> {
    let mut out: Vec<(String, Option<PathBuf>)> = builtin_names()
        .map(|name| (name.to_owned(), None))
        .collect();
    let mut user: Vec<(String, PathBuf)> = themes_dir()
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            (!name.starts_with('.')).then(|| (name, e.path()))
        })
        .collect();
    user.sort();
    for (name, path) in user {
        match out.iter_mut().find(|(n, _)| *n == name) {
            Some(builtin) => builtin.1 = Some(path),
            None => out.push((name, Some(path))),
        }
    }
    out
}

/// The theme after `current` in `names`, or before it with `back`,
/// wrapping around; the first when `current` isn't one of them.
pub(crate) fn step_name<'a>(names: &[&'a str], current: &str, back: bool) -> Option<&'a str> {
    let len = names.len();
    let next = match names.iter().position(|n| *n == current) {
        Some(i) if back => (i + len - 1) % len,
        Some(i) => (i + 1) % len,
        None => 0,
    };
    names.get(next).copied()
}

/// The titles of the groups `dffy themes` and the viewer's theme list sort
/// themes into, by [`Theme::group`].
pub(crate) const GROUPS: [&str; 3] = ["Terminal palette", "Dark", "Light"];

/// `dffy themes`: the themes grouped by the background they're made for,
/// each with its origin and a sample diff drawn in it, tinted when the
/// indicators are.
pub(crate) fn list(ctx: &crate::Ctx, out: &mut crate::output::Output) -> std::io::Result<()> {
    let indicators = ctx.indicators();
    let terminal = if ctx.config.background == config::Background::Light {
        LIGHT_BACKGROUND
    } else {
        DARK_BACKGROUND
    };
    let mut groups = GROUPS.map(|title| (title, Vec::new()));
    for (name, path) in available() {
        let mut warnings = Vec::new();
        let mut theme =
            Theme::load(&name, &mut warnings).unwrap_or_else(|_| Theme::default_theme());
        if indicators == Indicators::Background {
            theme.tint(terminal);
        }
        theme.on_own_background();
        groups[theme.group()]
            .1
            .push((name, path, theme, warnings.len()));
    }
    let mut c = Canvas::default();
    for (i, (title, themes)) in groups.iter().filter(|(_, t)| !t.is_empty()).enumerate() {
        if i > 0 {
            c.end_line();
        }
        c.line(Role::Hint, title);
        for (name, path, theme, problems) in themes {
            c.push(Role::Ok, if *name == ctx.theme.name { "* " } else { "  " });
            c.push(Role::FileHeader, name);
            c.push(
                Role::Dim,
                &match path {
                    Some(path) => format!("  {}", path.display()),
                    None => "  built-in".to_owned(),
                },
            );
            if *problems > 0 {
                c.push(Role::Warn, &format!("  {problems} problem(s)"));
            }
            c.end_line();
            out.write(&c, ctx.out_theme())?;
            c.clear();
            preview(
                &mut c,
                indicators,
                (ctx.color_out && theme.background.is_some()).then_some(PREVIEW_WIDTH),
            );
            out.write(&c, ctx.color_out.then_some(theme))?;
            c.clear();
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sample {
    Context,
    Removed,
    Added,
    Moved,
}

/// A piece of a sample line: its syntax, whether it's a changed token,
/// and its text.
type Piece = (Option<Syntax>, bool, &'static str);

const SAMPLE: [(Sample, u32, &[Piece]); 4] = [
    (
        Sample::Context,
        12,
        &[
            (Some(Syntax::Keyword), false, "fn"),
            (None, false, " "),
            (Some(Syntax::Function), false, "total"),
            (None, false, "(items: &["),
            (Some(Syntax::Type), false, "u32"),
            (None, false, "]) -> "),
            (Some(Syntax::Type), false, "u32"),
            (None, false, " {"),
        ],
    ),
    (
        Sample::Removed,
        13,
        &[
            (None, false, "    "),
            (Some(Syntax::Keyword), false, "let"),
            (None, false, " sum = "),
            (None, true, "count"),
            (None, false, " + "),
            (Some(Syntax::Number), false, "1"),
            (None, false, ";"),
        ],
    ),
    (
        Sample::Added,
        13,
        &[
            (None, false, "    "),
            (Some(Syntax::Keyword), false, "let"),
            (None, false, " sum = "),
            (None, true, "items."),
            (Some(Syntax::Function), true, "len"),
            (None, true, "()"),
            (None, false, " + "),
            (Some(Syntax::Number), false, "1"),
            (None, false, ";"),
        ],
    ),
    (
        Sample::Moved,
        14,
        &[
            (None, false, "    "),
            (Some(Syntax::Function), false, "log"),
            (None, false, "("),
            (Some(Syntax::String), false, "\"total\""),
            (None, false, ", sum);"),
        ],
    ),
];

/// The columns a padded sample takes, so a theme's background shows as a
/// block.
const PREVIEW_WIDTH: usize = 48;

/// The sample diff as the viewer draws it with `indicators`, padded to
/// `width` columns when given.
fn preview(c: &mut Canvas, indicators: Indicators, width: Option<usize>) {
    for (kind, number, pieces) in SAMPLE {
        let (text, emph, sign, number_role, sign_char) = match kind {
            Sample::Context => (
                Role::Context,
                Role::Context,
                Role::Context,
                Role::LineNumber,
                ' ',
            ),
            Sample::Removed => (
                Role::RemovedLine,
                Role::RemovedEmph,
                Role::Removed,
                Role::RemovedNumber,
                '-',
            ),
            Sample::Added => (
                Role::AddedLine,
                Role::AddedEmph,
                Role::Added,
                Role::AddedNumber,
                '+',
            ),
            Sample::Moved => (Role::Moved, Role::Moved, Role::Moved, Role::Moved, '»'),
        };
        let changed = kind != Sample::Context;
        let tinted = changed && indicators == Indicators::Background;
        c.push(Role::Plain, "  ");
        c.push(number_role, &format!(" {number:>3}"));
        let mark = match indicators {
            Indicators::Background if changed => {
                c.push(number_role, " │");
                c.push(text, " ");
                3
            }
            Indicators::Bars if changed => {
                c.push(Role::Plain, " ");
                c.push(sign, "▌");
                c.push(Role::Plain, " ");
                3
            }
            Indicators::Classic => {
                c.push(Role::Plain, " ");
                c.push_char(sign, sign_char);
                c.push(Role::Dim, "│");
                c.push(Role::Plain, " ");
                4
            }
            Indicators::Auto | Indicators::Background | Indicators::Bars | Indicators::None => {
                c.push(Role::Dim, " │");
                c.push(Role::Plain, " ");
                3
            }
        };
        let mut used = 6 + mark;
        for &(syntax, token, piece) in pieces {
            let role = if token { emph } else { text };
            c.push_ink(
                Ink {
                    role,
                    syntax,
                    attributes: 0,
                },
                piece,
            );
            used += piece.chars().count();
        }
        if let Some(width) = width {
            c.spaces(
                if tinted { text } else { Role::Plain },
                width.saturating_sub(used),
            );
        }
        c.end_line();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Red and green mixed into the background at each share come out
    /// equally light (Oklab lightness, which follows what eyes see), on
    /// dark terminals and light ones.
    #[test]
    fn red_and_green_tints_are_equally_light() {
        let hue = |h| color::oklch(HUE_LIGHTNESS, HUE_CHROMA, h);
        let lightness = |rgb| color::to_oklab(rgb)[0];
        for bg in [
            DARK_BACKGROUND,
            LIGHT_BACKGROUND,
            (0, 0, 0),
            (0x28, 0x2c, 0x34),
        ] {
            for share in [LINE_TINT, GUTTER_TINT, EMPH_TINT] {
                let red = lightness(color::blend(bg, hue(REMOVED_HUE), share));
                let green = lightness(color::blend(bg, hue(ADDED_HUE), share));
                assert!(
                    (red - green).abs() < 0.015,
                    "{bg:?} at {share}: {red} against {green}"
                );
            }
        }
        // And by WCAG relative luminance on the two assumed backgrounds.
        for bg in [DARK_BACKGROUND, LIGHT_BACKGROUND] {
            for share in [LINE_TINT, GUTTER_TINT, EMPH_TINT] {
                let red = color::luminance(color::blend(bg, hue(REMOVED_HUE), share));
                let green = color::luminance(color::blend(bg, hue(ADDED_HUE), share));
                let apart = (red - green).abs() / red.max(green);
                assert!(apart < 0.15, "{bg:?} at {share}: {red} against {green}");
            }
        }
    }

    /// Line numbers read on their gutter's tint (WCAG AA, 4.5:1), and the
    /// tints follow the background the terminal reports.
    #[test]
    fn line_numbers_read_on_their_tint() {
        for bg in [
            DARK_BACKGROUND,
            LIGHT_BACKGROUND,
            (0x28, 0x2c, 0x34),
            (0xfd, 0xf6, 0xe3),
        ] {
            let mut theme = Theme::default_theme();
            theme.tint(bg);
            for role in [Role::AddedNumber, Role::RemovedNumber, Role::ModifiedNumber] {
                let style = theme.style(role);
                let (Some(Color::Rgb(fr, fg, fb)), Some(Color::Rgb(br, bgr, bb))) =
                    (style.fg, style.bg)
                else {
                    panic!("{role:?} on {bg:?}: {style:?}");
                };
                let ratio = color::contrast((fr, fg, fb), (br, bgr, bb));
                assert!(ratio >= 4.5, "{role:?} on {bg:?}: {ratio}");
            }
        }
        let tinted = |bg| {
            let mut theme = Theme::default_theme();
            theme.tint(bg);
            theme.style(Role::AddedLine).bg
        };
        assert_ne!(tinted(DARK_BACKGROUND), tinted(LIGHT_BACKGROUND));
    }

    #[test]
    fn parses_names_hex_background_and_attributes() {
        let style = Style::parse("bold bright-green on #102030").expect("valid");
        assert_eq!(style.fg, Some(Color::Ansi(10)));
        assert_eq!(style.bg, Some(Color::Rgb(0x10, 0x20, 0x30)));
        assert_eq!(style.sgr(), "\x1b[0;1;92;48;2;16;32;48m");
        assert_eq!(Style::parse("").expect("valid").sgr(), "");
        assert!(Style::parse("grean").is_err());
        assert!(Style::parse("red on").is_err());
    }

    #[test]
    fn default_theme_uses_only_the_ansi_palette() {
        let theme = Theme::default_theme();
        for role in Role::THEMED {
            let style = theme.style(role);
            for color in [style.fg, style.bg].into_iter().flatten() {
                assert!(matches!(color, Color::Ansi(_)), "{role:?} uses {color:?}");
            }
        }
        assert_eq!(theme.sgr(Role::Added.into()), "\x1b[0;32m");
        for syntax in Syntax::ALL {
            let style = theme.ink_style(Ink {
                role: Role::AddedLine,
                syntax: Some(syntax),
                attributes: 0,
            });
            assert!(style.fg.is_none_or(|c| matches!(c, Color::Ansi(_))));
        }
    }

    #[test]
    fn syntax_colors_yield_to_reversed_emphasis_and_keep_line_backgrounds() {
        let theme = Theme::parse(
            "t",
            "added-emph = green reverse\nadded-line = on #102030\nsyntax-keyword = bold magenta",
            &mut Vec::new(),
        );
        let ink = |role| Ink {
            role,
            syntax: Some(Syntax::Keyword),
            attributes: 0,
        };
        assert_eq!(
            theme.ink_style(ink(Role::AddedEmph)),
            theme.style(Role::AddedEmph)
        );
        let line = theme.ink_style(ink(Role::AddedLine));
        assert_eq!(line.fg, Some(Color::Ansi(5)));
        assert_eq!(line.bg, Some(Color::Rgb(0x10, 0x20, 0x30)));
        assert_eq!(line.attributes, 1);
        assert_eq!(
            theme.sgr(ink(Role::AddedLine)),
            "\x1b[0;1;35;48;2;16;32;48m"
        );
    }

    fn builtin(name: &str) -> Theme {
        Theme::parse(
            name,
            &builtin_source(name).expect("built-in"),
            &mut Vec::new(),
        )
    }

    #[test]
    fn builtin_themes_set_every_key_without_warnings() {
        assert_eq!(builtin_names().count(), 16);
        for name in builtin_names() {
            let source = builtin_source(name).expect("listed");
            let mut warnings = Vec::new();
            Theme::parse(name, &source, &mut warnings);
            assert!(warnings.is_empty(), "{name}: {warnings:?}");
            let set: Vec<&str> = source
                .lines()
                .filter(|l| !l.starts_with('#'))
                .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim()))
                .collect();
            assert_eq!(set, keys(), "{name}");
        }
    }

    /// Every built-in under background tints, on a dark terminal and a
    /// light one: line numbers read on their tint (WCAG AA), red, green and
    /// modified tints are equally light, code reads on its line's tint, and
    /// a theme made for a background tints into that one, whatever the
    /// terminal's.
    #[test]
    fn every_builtin_tints_readably_and_evenly() {
        let rgb = |color: Option<Color>| match color {
            Some(Color::Rgb(r, g, b)) => (r, g, b),
            other => panic!("{other:?} is not truecolor"),
        };
        let lightness = |rgb| color::to_oklab(rgb)[0];
        for name in builtin_names() {
            for terminal in [DARK_BACKGROUND, LIGHT_BACKGROUND] {
                let mut theme = builtin(name);
                theme.tint(terminal);
                let under = theme.background.unwrap_or(terminal);
                for role in [Role::AddedNumber, Role::RemovedNumber, Role::ModifiedNumber] {
                    let style = theme.style(role);
                    let ratio = color::contrast(rgb(style.fg), rgb(style.bg));
                    assert!(ratio >= 4.5, "{name} {role:?} on {terminal:?}: {ratio}");
                }
                for (added, removed) in [
                    (Role::AddedLine, Role::RemovedLine),
                    (Role::AddedNumber, Role::RemovedNumber),
                    (Role::AddedEmph, Role::RemovedEmph),
                    (Role::AddedLine, Role::ModifiedLine),
                    (Role::AddedNumber, Role::ModifiedNumber),
                ] {
                    let green = lightness(rgb(theme.style(added).bg));
                    let red = lightness(rgb(theme.style(removed).bg));
                    assert!(
                        (red - green).abs() < 0.015,
                        "{name} {added:?} on {terminal:?}: {red} against {green}"
                    );
                }
                assert_eq!(
                    color::is_light(rgb(theme.style(Role::AddedLine).bg)),
                    color::is_light(under),
                    "{name} on {terminal:?}"
                );
                // 3:1, not 4.5: Solarized's own body text is 4.75:1 on its
                // background before any tint.
                if let Some(Color::Rgb(r, g, b)) = theme.style(Role::AddedLine).fg {
                    for role in [Role::AddedLine, Role::RemovedLine, Role::ModifiedLine] {
                        let ratio = color::contrast((r, g, b), rgb(theme.style(role).bg));
                        assert!(ratio >= 3.0, "{name} {role:?} text: {ratio}");
                    }
                }
            }
        }
    }

    /// Modified lines take their tint's hue from the theme's own blue, the
    /// `modified` color; a theme file that predates the modified roles
    /// loads without warnings and gets them from what it inherits.
    #[test]
    fn modified_tints_take_the_themes_blue() {
        let rgb = |color: Option<Color>| match color {
            Some(Color::Rgb(r, g, b)) => (r, g, b),
            other => panic!("{other:?} is not truecolor"),
        };
        for name in ["github-dark", "gruvbox-dark", "solarized-light"] {
            let mut theme = builtin(name);
            theme.tint(DARK_BACKGROUND);
            let bg = theme.background.unwrap_or(DARK_BACKGROUND);
            let blue = rgb(theme.style(Role::Modified).fg);
            let hue = color::oklch(HUE_LIGHTNESS, HUE_CHROMA, color::hue(blue));
            let expected = color::blend(bg, hue, GUTTER_TINT);
            assert_eq!(
                rgb(theme.style(Role::ModifiedNumber).bg),
                expected,
                "{name}"
            );
        }
        let mut warnings = Vec::new();
        let old = Theme::parse("old", "inherit = nord\nadded = #00ff00\n", &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            old.style(Role::Modified),
            builtin("nord").style(Role::Modified)
        );
    }

    /// In every built-in, modified lines never take the hue of added,
    /// removed or moved ones: neither their tint nor their own color.
    #[test]
    fn modified_lines_keep_a_hue_of_their_own() {
        let apart = |a: f64, b: f64| {
            let d = (a - b).rem_euclid(360.0);
            d.min(360.0 - d)
        };
        for name in builtin_names() {
            let mut theme = builtin(name);
            theme.tint(DARK_BACKGROUND);
            let hue = theme.modified_hue();
            for other in [ADDED_HUE, REMOVED_HUE, MOVED_HUE] {
                assert!(apart(hue, other) >= 25.0, "{name}: {hue} near {other}");
            }
            if let (Some(Color::Rgb(r, g, b)), Some(Color::Rgb(mr, mg, mb))) =
                (theme.style(Role::Modified).fg, theme.style(Role::Moved).fg)
            {
                let (own, moved) = (color::hue((r, g, b)), color::hue((mr, mg, mb)));
                assert!(
                    apart(own, moved) >= 25.0,
                    "{name}: {own} near moved {moved}"
                );
            }
        }
    }

    /// Search matches and the status bar's chip read in every truecolor
    /// built-in, light ones included: 4.5:1, or 3:1 in a palette whose
    /// body text is itself under 5:1 on its background (Solarized).
    #[test]
    fn chips_read_on_their_background() {
        let rgb = |color: Option<Color>| match color {
            Some(Color::Rgb(r, g, b)) => (r, g, b),
            other => panic!("{other:?} is not truecolor"),
        };
        for name in builtin_names() {
            let theme = builtin(name);
            let Some(bg) = theme.background else {
                continue;
            };
            let body = color::contrast(rgb(theme.style(Role::Context).fg), bg);
            let floor = if body < 5.0 { 3.0 } else { 4.5 };
            for role in [Role::SearchMatch, Role::SearchCurrent, Role::StatusTag] {
                let style = theme.style(role);
                let ratio = color::contrast(rgb(style.fg), rgb(style.bg));
                assert!(ratio >= floor, "{name} {role:?}: {ratio}");
            }
        }
    }

    /// A truecolor theme's selection sits between its background and its
    /// text, lighter on dark themes and darker on light ones, the inactive
    /// one closer to the background. The name, status letters and counts on
    /// it keep 4.5:1, or for a palette color already under that on the
    /// plain background, at least four fifths of its contrast there.
    #[test]
    fn selections_lift_gently_and_keep_row_text_readable() {
        let rgb = |color: Option<Color>| match color {
            Some(Color::Rgb(r, g, b)) => (r, g, b),
            other => panic!("{other:?} is not truecolor"),
        };
        let lightness = |rgb| color::to_oklab(rgb)[0];
        let row_text = [
            Role::Context,
            Role::Added,
            Role::Removed,
            Role::Warn,
            Role::Hint,
            Role::Dim,
        ];
        for name in builtin_names() {
            let theme = builtin(name);
            let Some(bg) = theme.background else {
                continue;
            };
            let text = rgb(theme.style(Role::Context).fg);
            let lift = |focused| lightness(rgb(theme.selected(Style::PLAIN, focused).bg));
            let (base, top) = (lightness(bg), lightness(text));
            let toward = |l: f64| (l - base) / (top - base);
            assert!(
                0.0 < toward(lift(false)) && toward(lift(false)) < toward(lift(true)),
                "{name}: {} then {}",
                toward(lift(false)),
                toward(lift(true))
            );
            assert!(toward(lift(true)) < 0.15, "{name}: {}", toward(lift(true)));
            for focused in [true, false] {
                let on = rgb(theme.selected(Style::PLAIN, focused).bg);
                for role in row_text {
                    let fg = rgb(theme.selected(theme.style(role), focused).fg);
                    let (plain, selected) = (color::contrast(fg, bg), color::contrast(fg, on));
                    assert!(
                        selected >= 4.5_f64.min(plain * 0.8),
                        "{name} {role:?} focused {focused}: {selected} (plain {plain})"
                    );
                }
            }
        }
    }

    #[test]
    fn inherit_starts_from_a_builtin_and_lines_override_it() {
        let mut warnings = Vec::new();
        let theme = Theme::parse(
            "mine",
            "added = #00ff00\nsyntax-keyword = bold red\ninherit = dracula\n",
            &mut warnings,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let dracula = builtin("dracula");
        assert_eq!(theme.style(Role::Added).fg, Some(Color::Rgb(0, 0xff, 0)));
        assert_eq!(
            theme.syntax[Syntax::Keyword as usize],
            Style::parse("bold red").expect("valid")
        );
        assert_eq!(theme.style(Role::Removed), dracula.style(Role::Removed));
        assert_eq!(
            theme.syntax[Syntax::String as usize],
            dracula.syntax[Syntax::String as usize]
        );
        assert_eq!(theme.background, dracula.background);

        let mut warnings = Vec::new();
        let theme = Theme::parse("mine", "inherit = dracla\n", &mut warnings);
        assert_eq!(theme.styles, Theme::default_theme().styles);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("did you mean 'dracula'?"),
            "{warnings:?}"
        );
    }

    #[test]
    fn unknown_keys_and_bad_backgrounds_warn_with_a_fix() {
        let mut warnings = Vec::new();
        Theme::parse("t", "adde = red\nbackground = red\n", &mut warnings);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert_eq!(warnings[0].line, 1);
        assert_eq!(warnings[0].message, "unknown key 'adde'");
        assert_eq!(warnings[0].hint.as_deref(), Some("did you mean 'added'?"));
        assert_eq!(warnings[1].line, 2);
        assert!(warnings[1].message.contains("#rrggbb"), "{warnings:?}");
    }

    /// A dump loaded back as a user theme is the built-in: plain, and
    /// tinted on either terminal background.
    #[test]
    fn dump_round_trips_through_the_theme_file_parser() {
        for name in builtin_names() {
            let dumped = dump(name).expect("built-in");
            let mut warnings = Vec::new();
            let mut copy = Theme::parse("copy", &dumped, &mut warnings);
            assert!(warnings.is_empty(), "{name}: {warnings:?}");
            let mut original = builtin(name);
            for terminal in [None, Some(DARK_BACKGROUND), Some(LIGHT_BACKGROUND)] {
                if let Some(bg) = terminal {
                    copy.tint(bg);
                    original.tint(bg);
                }
                assert_eq!(copy.styles, original.styles, "{name} {terminal:?}");
                assert_eq!(copy.syntax, original.syntax, "{name}");
                assert_eq!(copy.background, original.background, "{name}");
                assert_eq!(copy.sgr, original.sgr, "{name}");
            }
        }
        assert_eq!(
            dump("dracla").expect_err("unknown"),
            "no built-in theme 'dracla'; did you mean 'dracula'?"
        );
    }

    #[test]
    fn cycling_visits_every_builtin_theme_in_order() {
        let names: Vec<&str> = builtin_names().collect();
        let mut seen = vec!["default"];
        while seen.len() <= names.len() {
            seen.push(step_name(&names, seen.last().expect("one"), false).expect("non-empty"));
        }
        insta::assert_snapshot!(
            seen.join(" → "),
            @"default → mono → github-dark → tokyo-night → catppuccin-mocha → gruvbox-dark → nord → dracula → one-dark → solarized-dark → rose-pine → kanagawa → github-light → catppuccin-latte → gruvbox-light → solarized-light → default"
        );
        assert_eq!(step_name(&names, "default", true), Some("solarized-light"));
        assert_eq!(step_name(&names, "mono", true), Some("default"));
        assert_eq!(step_name(&names, "gone", true), Some("default"));
        assert_eq!(step_name(&[], "default", false), None);
    }

    #[test]
    fn preview_draws_the_sample_in_the_theme_on_its_background() {
        let mut theme = builtin("dracula");
        theme.tint(LIGHT_BACKGROUND);
        theme.on_own_background();
        let mut c = Canvas::default();
        preview(&mut c, Indicators::Background, Some(PREVIEW_WIDTH));
        let mut out = Vec::new();
        c.write_to(&mut out, Some(&theme)).expect("write");
        let text = String::from_utf8(out).expect("utf-8").replace('\x1b', "␛");
        insta::assert_snapshot!(text);
    }
}

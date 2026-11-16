//! What a frontend draws for a diff, short of how it looks: the lines of a
//! hunk with what each one is ([`LineState`]), its headers, the rows a hunk
//! is arranged into, and the syntax runs and paint that style a line's
//! text. Nothing here knows widths, colors or cells; a frontend measures,
//! wraps and colors these itself.
//!
//! These leaf types are plain data over borrowed slices and depend on
//! nothing above [`crate::diff`]: no git, no parser, no threads or I/O.

use std::ops::Range;

use crate::diff::{LineKind, Text, Unchanged};

/// What a highlighted stretch of text is. A highlighter maps its grammar's
/// capture names onto these; a theme gives each one a style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Syntax {
    /// Keywords, conditionals, loops, includes and storage classes; also
    /// markup headings.
    Keyword,
    /// String and character literals; also markup code spans.
    String,
    /// Escape sequences inside strings.
    Escape,
    /// Comments of every kind.
    Comment,
    /// Function and method names; also markup links.
    Function,
    /// Type names and constructors.
    Type,
    /// Integer and float literals.
    Number,
    /// Constants and booleans.
    Constant,
    /// Operators.
    Operator,
    /// Brackets, delimiters and separators.
    Punctuation,
    /// Properties, fields and mapping keys.
    Property,
    /// Built-in variables such as `self` or `this`.
    VariableBuiltin,
    /// Attributes and annotations.
    Attribute,
    /// Markup tags.
    Tag,
    /// Labels, as for `goto`.
    Label,
}

impl Syntax {
    /// Every role, in discriminant order. Disk cache keys include each
    /// role's [`Syntax::key`], so changing this invalidates cached spans.
    pub const ALL: [Self; 15] = [
        Self::Keyword,
        Self::String,
        Self::Escape,
        Self::Comment,
        Self::Function,
        Self::Type,
        Self::Number,
        Self::Constant,
        Self::Operator,
        Self::Punctuation,
        Self::Property,
        Self::VariableBuiltin,
        Self::Attribute,
        Self::Tag,
        Self::Label,
    ];

    /// The name themes use, after a `syntax-` prefix.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Keyword => "keyword",
            Self::String => "string",
            Self::Escape => "escape",
            Self::Comment => "comment",
            Self::Function => "function",
            Self::Type => "type",
            Self::Number => "number",
            Self::Constant => "constant",
            Self::Operator => "operator",
            Self::Punctuation => "punctuation",
            Self::Property => "property",
            Self::VariableBuiltin => "variable-builtin",
            Self::Attribute => "attribute",
            Self::Tag => "tag",
            Self::Label => "label",
        }
    }

    pub(crate) fn from_u8(n: u8) -> Option<Self> {
        Self::ALL.get(usize::from(n)).copied()
    }
}

/// A highlighted stretch of one line, as byte offsets from the line's start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// First byte, inclusive.
    pub start: u32,
    /// Past the last byte; runs on one line never overlap and come in order.
    pub end: u32,
    /// What the text is.
    pub syntax: Syntax,
}

/// Syntax runs and leaf tokens for a range of lines of one side, as a
/// highlighter produces them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Spans {
    pub(crate) first: usize,
    /// For each line, the index of its first run, plus a final entry.
    pub(crate) line_runs: Vec<u32>,
    pub(crate) runs: Vec<Run>,
    /// Leaf nodes as byte ranges of the whole text, in order, clipped to
    /// the lines.
    pub(crate) leaves: Vec<(u32, u32)>,
}

impl Spans {
    /// The lines covered.
    #[must_use]
    #[inline]
    pub fn lines(&self) -> Range<usize> {
        self.first..self.first + self.line_runs.len().saturating_sub(1)
    }

    /// The runs of line `i`; none outside [`Spans::lines`].
    #[must_use]
    #[inline]
    pub fn line(&self, i: usize) -> &[Run] {
        if !self.lines().contains(&i) {
            return &[];
        }
        let k = i - self.first;
        &self.runs[self.line_runs[k] as usize..self.line_runs[k + 1] as usize]
    }

    /// The spans of `lines`, which must lie within [`Spans::lines`], of
    /// `text`, the side these spans are of.
    #[must_use]
    pub fn slice(&self, text: &Text, lines: Range<usize>) -> Self {
        let k = lines.start - self.first..lines.end - self.first;
        let first_run = self.line_runs[k.start];
        let (start, end) = (
            len_u32(text.offset(lines.start)),
            len_u32(text.offset(lines.end)),
        );
        let from = self.leaves.partition_point(|&(_, e)| e <= start);
        let to = self.leaves.partition_point(|&(s, _)| s < end);
        Self {
            first: lines.start,
            line_runs: self.line_runs[k.start..=k.end]
                .iter()
                .map(|&r| r - first_run)
                .collect(),
            runs: self.runs[first_run as usize..self.line_runs[k.end] as usize].to_vec(),
            leaves: self.leaves[from..to]
                .iter()
                .map(|&(s, e)| (s.max(start), e.min(end)))
                .collect(),
        }
    }

    /// Token boundaries for intra-line emphasis on line `i` of `text`
    /// (without its terminator): each leaf node is a token, except that
    /// leaves holding whitespace (strings, comments) and the gaps between
    /// leaves are split further by [`crate::diff::tokenize`]. Lines outside
    /// [`Spans::lines`] are tokenized plainly.
    #[must_use]
    pub fn tokens(&self, text: &Text, i: usize) -> Vec<Range<usize>> {
        use crate::diff::tokenize;
        let line = text.line(i);
        let content = &line[..content_len(line)];
        if !self.lines().contains(&i) {
            return tokenize(content);
        }
        let start = text.offset(i);
        let end = start + content.len();
        let mut out = Vec::new();
        let mut pos = 0;
        let split = |out: &mut Vec<Range<usize>>, from: usize, to: usize| {
            out.extend(
                tokenize(&content[from..to])
                    .into_iter()
                    .map(|r| r.start + from..r.end + from),
            );
        };
        let first = self.leaves.partition_point(|&(_, e)| e as usize <= start);
        for &(s, e) in &self.leaves[first..] {
            let (s, e) = (s as usize, e as usize);
            if s >= end {
                break;
            }
            let (from, to) = (s.max(start) - start, e.min(end) - start);
            if from >= to || from < pos {
                continue;
            }
            if from > pos {
                split(&mut out, pos, from);
            }
            if content[from..to].iter().any(u8::is_ascii_whitespace) {
                split(&mut out, from, to);
            } else {
                out.push(from..to);
            }
            pos = to;
        }
        if pos < content.len() {
            split(&mut out, pos, content.len());
        }
        out
    }
}

#[expect(
    clippy::expect_used,
    reason = "spans come only from a highlighter, which refuses sides over its byte cap (512 KiB), and are read against that same text, so offsets and run counts stay far below 2^32"
)]
pub(crate) fn len_u32(n: usize) -> u32 {
    u32::try_from(n).expect("highlighted text is under the highlighter's byte cap")
}

/// The bytes of `line` before its terminator (`\n` or `\r\n`).
pub(crate) fn content_len(line: &[u8]) -> usize {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line).len()
}

/// Text attributes, one bit each: bold 1, dim 2, italic 4, underline 8,
/// reverse 16. A frontend's styles can hold the same bits, so drawing
/// combines [`Paint`] with them without converting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Attributes(pub u8);

impl Attributes {
    /// No attribute.
    pub const NONE: Self = Self(0);
    /// Bold.
    pub const BOLD: Self = Self(1);
    /// Dim (faint).
    pub const DIM: Self = Self(1 << 1);
    /// Italic.
    pub const ITALIC: Self = Self(1 << 2);
    /// Underlined.
    pub const UNDERLINE: Self = Self(1 << 3);
    /// Foreground and background swapped.
    pub const REVERSE: Self = Self(1 << 4);

    /// Whether every attribute of `other` is set.
    #[inline]
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Attributes {
    type Output = Self;

    #[inline]
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOrAssign for Attributes {
    #[inline]
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// A stretch of a rendered line (from a renderer plugin) in its own
/// style: a syntax color and attributes. Offsets are bytes of the line;
/// a line's paint comes in order and doesn't overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paint {
    /// First byte, inclusive.
    pub start: u32,
    /// Past the last byte.
    pub end: u32,
    /// The syntax color, if any.
    pub syntax: Option<Syntax>,
    /// Attributes on top of the line's style.
    pub attributes: Attributes,
}

/// Where a displayed line of a hunk comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// On both sides, unchanged.
    Context,
    /// Only on the old side.
    Removed,
    /// Only on the new side.
    Added,
    /// The `\ No newline at end of file` marker after the line it refers to.
    NoNewline,
}

/// What a displayed line of a hunk is, as the diff found it. Nothing here
/// says how it looks: a paired line shows as modified only where the
/// frontend's modified-lines setting asks for that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LineState {
    /// On both sides, unchanged.
    Context,
    /// Only on the old side, not paired.
    Removed,
    /// Only on the new side, not paired.
    Added,
    /// Paired with an added line as its edited version
    /// ([`crate::diff::Line::paired`]).
    RemovedPaired,
    /// Paired with a removed line as its edited version.
    AddedPaired,
    /// The syntax diff found its code among the other side's changes:
    /// moved, not edited.
    RemovedMoved,
    /// An added line the syntax diff found moved from elsewhere.
    AddedMoved,
    /// The syntax diff found the same code on the other side in its block:
    /// only whitespace or line breaks changed.
    RemovedReformatted,
    /// An added line the syntax diff found only reformatted.
    AddedReformatted,
    /// The `\ No newline at end of file` marker after the line before it.
    NoNewline,
}

impl LineState {
    /// A line of `kind`, `paired` with its edit or not. What the syntax
    /// diff found `unchanged` outranks the pairing; context lines and the
    /// marker are never either.
    #[inline]
    #[must_use]
    pub fn new(kind: Kind, paired: bool, unchanged: Unchanged) -> Self {
        match (kind, unchanged, paired) {
            (Kind::Context, ..) => Self::Context,
            (Kind::NoNewline, ..) => Self::NoNewline,
            (Kind::Removed, Unchanged::Moved, _) => Self::RemovedMoved,
            (Kind::Added, Unchanged::Moved, _) => Self::AddedMoved,
            (Kind::Removed, Unchanged::Layout, _) => Self::RemovedReformatted,
            (Kind::Added, Unchanged::Layout, _) => Self::AddedReformatted,
            (Kind::Removed, Unchanged::No, true) => Self::RemovedPaired,
            (Kind::Added, Unchanged::No, true) => Self::AddedPaired,
            (Kind::Removed, Unchanged::No, false) => Self::Removed,
            (Kind::Added, Unchanged::No, false) => Self::Added,
        }
    }

    /// The state of a line of a core hunk.
    #[inline]
    #[must_use]
    pub fn of(line: &crate::diff::Line) -> Self {
        let kind = match line.kind {
            LineKind::Context => Kind::Context,
            LineKind::Removed => Kind::Removed,
            LineKind::Added => Kind::Added,
        };
        Self::new(kind, line.paired, line.unchanged)
    }

    /// Where the line comes from.
    #[inline]
    #[must_use]
    pub fn kind(self) -> Kind {
        match self {
            Self::Context => Kind::Context,
            Self::Removed | Self::RemovedPaired | Self::RemovedMoved | Self::RemovedReformatted => {
                Kind::Removed
            }
            Self::Added | Self::AddedPaired | Self::AddedMoved | Self::AddedReformatted => {
                Kind::Added
            }
            Self::NoNewline => Kind::NoNewline,
        }
    }

    /// Moved or reformatted: code the syntax diff found unchanged.
    #[inline]
    #[must_use]
    pub fn is_unchanged(self) -> bool {
        matches!(
            self,
            Self::RemovedMoved
                | Self::AddedMoved
                | Self::RemovedReformatted
                | Self::AddedReformatted
        )
    }
}

/// A displayed line of a hunk, borrowing everything it shows.
#[derive(Debug, Clone, Copy)]
pub struct Line<'a> {
    /// What the line is.
    pub state: LineState,
    /// 1-based line number on the old side; `None` when it isn't there.
    pub old: Option<usize>,
    /// 1-based line number on the new side; `None` when it isn't there.
    pub new: Option<usize>,
    /// The line's content without its terminator.
    pub text: &'a [u8],
    /// Byte ranges of `text` to emphasize.
    pub emphasis: &'a [Range<usize>],
    /// Syntax runs of `text`, when the line is highlighted. Its text then
    /// takes the syntax colors instead of the added or removed color.
    pub syntax: Option<&'a [Run]>,
    /// A renderer plugin's styles for `text`; empty on every other line.
    /// Like syntax runs, they color the text instead of the change.
    pub paint: &'a [Paint],
    /// Split arrangement starts a new block of rows here (see
    /// [`crate::diff::Line::block_start`]).
    pub block_start: bool,
}

/// Numbers as git prints them in `@@ -a,b +c,d @@`.
#[derive(Debug, Clone, Copy)]
pub struct HunkHeader<'a> {
    /// First old line, 1-based (the line before, for an empty range).
    pub old_start: usize,
    /// Old lines covered.
    pub old_len: usize,
    /// First new line, 1-based (the line before, for an empty range).
    pub new_start: usize,
    /// New lines covered.
    pub new_len: usize,
    /// The function context after the second `@@`.
    pub heading: &'a [u8],
    /// Unchanged lines left out before the hunk, when known: the header
    /// then shows as a separator saying so.
    pub hidden: usize,
}

impl HunkHeader<'_> {
    /// `@@ -a,b +c,d @@`, as git prints it.
    #[must_use]
    pub fn numbers(&self) -> String {
        let range = |start: usize, len: usize| {
            if len == 1 {
                start.to_string()
            } else {
                format!("{start},{len}")
            }
        };
        format!(
            "@@ -{} +{} @@",
            range(self.old_start, self.old_len),
            range(self.new_start, self.new_len)
        )
    }
}

/// How a hunk's lines are arranged into rows: one column, or old and new
/// side by side. Which one a hunk gets is the frontend's choice; rows for
/// either are the same whatever the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrangement {
    /// One column, every line in order.
    Unified,
    /// Old on the left, new on the right; the removed and added lines of a
    /// block pair up in order.
    Split,
}

/// A row of a hunk as arranged, before a frontend wraps it into screen
/// rows. Indices are lines of the hunk body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// The hunk's header.
    Header,
    /// Unified: one line, including the no-newline marker.
    Line(usize),
    /// Split: a context line on both sides, or a removed/added pair.
    Pair(Option<usize>, Option<usize>),
    /// Split: the no-newline marker.
    Marker,
}

/// Appends the rows of a hunk body of `len` lines in `arrangement` to
/// `rows`, without the header row, given each line's kind and whether it
/// starts a block of rows.
#[inline]
pub fn rows(
    arrangement: Arrangement,
    len: usize,
    line: impl Fn(usize) -> (Kind, bool),
    rows: &mut Vec<Row>,
) {
    match arrangement {
        Arrangement::Split => split_rows(len, line, rows),
        Arrangement::Unified => rows.extend((0..len).map(Row::Line)),
    }
}

/// Pairs the lines of a hunk into split rows.
fn split_rows(len: usize, line: impl Fn(usize) -> (Kind, bool), rows: &mut Vec<Row>) {
    let kind = |i| line(i).0;
    let mut i = 0;
    let mut added = Vec::new();
    while i < len {
        match kind(i) {
            Kind::Context => {
                rows.push(Row::Pair(Some(i), Some(i)));
                i += 1;
            }
            Kind::NoNewline => {
                rows.push(Row::Marker);
                i += 1;
            }
            Kind::Removed | Kind::Added => {
                let start = i;
                let continues = |i: usize| i == start || !line(i).1;
                while i < len && kind(i) == Kind::Removed && continues(i) {
                    i += 1;
                }
                let removed = start..i;
                added.clear();
                let mut marker = false;
                while i < len && matches!(kind(i), Kind::Added | Kind::NoNewline) && continues(i) {
                    if kind(i) == Kind::Added {
                        added.push(i);
                    } else {
                        marker = true;
                    }
                    i += 1;
                }
                for k in 0..removed.len().max(added.len()) {
                    rows.push(Row::Pair(
                        Some(removed.start + k).filter(|r| removed.contains(r)),
                        added.get(k).copied(),
                    ));
                }
                if marker {
                    rows.push(Row::Marker);
                }
            }
        }
    }
}

/// How a file changed, as its header names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// Only on the new side.
    Added,
    /// Only on the old side.
    Deleted,
    /// On both sides.
    Modified,
    /// Moved from another path, maybe edited.
    Renamed,
    /// Copied from another path, maybe edited.
    Copied,
    /// Changed between regular file, symlink and submodule.
    TypeChange,
    /// New and not yet tracked.
    Untracked,
    /// A merge's combined diff (`diff --cc`), passed through unpaired.
    Combined,
    /// Still conflicted in the index, shown against the working-tree file.
    Unmerged,
}

impl FileStatus {
    /// How a file header names it: `new file`, `modified`.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Added => "new file",
            Self::Deleted => "deleted file",
            Self::Modified => "modified",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
            Self::TypeChange => "type changed",
            Self::Untracked => "untracked file",
            Self::Combined => "merged",
            Self::Unmerged => "conflicted",
        }
    }
}

/// Where an uncommitted change sits, as `git status` splits it: `HEAD` to
/// the index is staged, the index to the working tree unstaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Staging {
    /// The index differs from `HEAD` and the working tree matches the index.
    Staged,
    /// The index matches `HEAD` and the working tree differs; untracked
    /// files too.
    Unstaged,
    /// Both differ: committing now would leave part of the change behind.
    Partly,
}

/// What a file's header says.
#[derive(Debug, Clone, Copy)]
pub struct FileHeader<'a> {
    /// How the file changed.
    pub status: FileStatus,
    /// The path on the old side; the same as `new_path` unless renamed or
    /// copied.
    pub old_path: &'a [u8],
    /// The path on the new side.
    pub new_path: &'a [u8],
    /// Octal mode on the old side, shown when both are known and differ.
    pub old_mode: Option<&'a str>,
    /// Octal mode on the new side.
    pub new_mode: Option<&'a str>,
    /// A rename's or copy's similarity, 0 to 100.
    pub similarity: Option<u8>,
    /// A line shown instead of hunks, such as "binary files differ".
    pub note: Option<&'a str>,
    /// Worded after the status (`modified · staged: path`); untracked
    /// files leave it out, since they can't be staged.
    pub staging: Option<Staging>,
}

/// Strips one trailing `\r`, so CRLF files don't show `^M` on every line.
#[inline]
#[must_use]
pub fn display_text(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// The bytes `line` shows, with its terminator when it has one: a removed
/// line's from the `old` side, any other's from the `new` one.
#[inline]
#[must_use]
pub fn line_bytes<'a>(old: &'a Text, new: &'a Text, line: &crate::diff::Line) -> &'a [u8] {
    match line.kind {
        LineKind::Removed => old.line(line.old),
        LineKind::Added | LineKind::Context => new.line(line.new),
    }
}

/// Whether `line` ends its side without a newline, so the `\ No newline
/// at end of file` marker follows it.
#[inline]
#[must_use]
pub fn no_newline(old: &Text, new: &Text, line: &crate::diff::Line) -> bool {
    !line_bytes(old, new, line).ends_with(b"\n")
}

/// git's default hunk heading: the nearest line above the hunk that starts
/// with a letter, `_` or `$`. Each search stops where the previous one
/// started, so asking for a file's hunks in order costs one backward pass
/// in total; asking out of order may miss a nearer line.
#[derive(Debug, Default)]
pub struct Heading {
    searched_to: usize,
    found: Option<usize>,
}

impl Heading {
    /// The heading for a hunk starting at old line `start` of `text`, as
    /// [`heading_text`] gives it; empty when there is none.
    pub fn find<'t>(&mut self, text: &'t Text, start: usize) -> &'t [u8] {
        self.find_line(text, start)
            .map_or(b"", |i| heading_text(text, i))
    }

    /// The index of the heading line for a hunk starting at old line `start`.
    pub fn find_line(&mut self, text: &Text, start: usize) -> Option<usize> {
        let start = start.min(text.len());
        if let Some(i) = (self.searched_to..start).rev().find(|&i| {
            text.line(i)
                .first()
                .is_some_and(|&b| b.is_ascii_alphabetic() || b == b'_' || b == b'$')
        }) {
            self.found = Some(i);
        }
        self.searched_to = self.searched_to.max(start);
        self.found
    }
}

/// Line `i` of `text` as a hunk heading: without trailing Unicode
/// whitespace, cut to 80 bytes.
#[must_use]
pub fn heading_text(text: &Text, i: usize) -> &[u8] {
    let line = text.line(i);
    let mut end = line.len();
    // The last character before `end`, when the bytes there are valid UTF-8.
    let last = |end: usize| {
        (1..=end.min(4)).find_map(|n| {
            let mut chars = std::str::from_utf8(&line[end - n..end]).ok()?.chars();
            let c = chars.next()?;
            chars.next().is_none().then_some(c)
        })
    };
    while let Some(c) = last(end).filter(|c| c.is_whitespace()) {
        end -= c.len_utf8();
    }
    &line[..end.min(80)]
}

#[cfg(test)]
mod tests;

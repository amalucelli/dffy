//! Unified diffs read as text, as `git diff`, `git show`, `git log -p` and
//! `diff -u` print them: a line-at-a-time [`Parser`] that turns them into
//! [`Event`]s, and a [`Builder`] that collects those into the commits and
//! files of the patch. The host reads the input and decides what to keep;
//! nothing here does I/O or spawns threads, and the parser keeps only the
//! current file's header, so its memory doesn't grow with the input.
//!
//! Hunk bodies are delimited by the line counts in their `@@` header, not by
//! what lines look like: a removed line reading `-- foo` is content, not a
//! file header.

use std::fmt;

use crate::diff::{self, LineKind, Text, Unchanged};
use crate::view::{FileStatus, HunkHeader, Kind};

/// What a line outside any file's hunks is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    /// Anything not listed below.
    Plain,
    /// `commit <id>`.
    CommitHeader,
    /// `Author:`, `Date:` and similar lines after a commit header.
    CommitMeta,
    /// Indented commit message lines.
    CommitMessage,
    /// Headers of other diff tools, such as `diff -ru a/x b/x`.
    Meta,
}

/// What [`Parser::feed`] found, borrowing the line fed or the parser's
/// file header.
#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    /// A line outside any file's hunks.
    Text(TextKind, &'a [u8]),
    /// A file's header is complete; sent before its first hunk.
    File(&'a FileInfo),
    /// A hunk's `@@` line.
    Hunk(Hunk<'a>),
    /// A hunk line without its prefix, or the whole `\ No newline` marker.
    /// A marker may also follow a finished hunk.
    Line(Kind, &'a [u8]),
    /// The hunk's counts are used up, or a line broke them.
    HunkEnd,
    /// A whole `+`, `-` or ` ` line inside a file but past its hunk's
    /// counts. It belongs to no hunk, so it's no change; the file goes on,
    /// and the next `@@` starts its next hunk.
    Stray(&'a [u8]),
    /// A line of a combined diff (`diff --cc`), passed through whole.
    /// `parents` is the number of prefix columns, 0 for the header lines
    /// before the first `@@@` (see [`combined_kind`]).
    Combined {
        /// Prefix columns of `line`.
        parents: usize,
        /// The whole line.
        line: &'a [u8],
    },
}

/// A hunk's `@@ -a,b +c,d @@ heading` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hunk<'a> {
    /// First old line, 1-based (the line before, for an empty range).
    pub old_start: usize,
    /// Old lines covered; 1 when the header leaves it out.
    pub old_len: usize,
    /// First new line, 1-based (the line before, for an empty range).
    pub new_start: usize,
    /// New lines covered; 1 when the header leaves it out.
    pub new_len: usize,
    /// The text after the second `@@`, without the space before it.
    pub heading: &'a [u8],
}

/// A file's header, as its `diff`, `---`/`+++` and extended header lines
/// give it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// How the file changed.
    pub status: FileStatus,
    /// The old path, unquoted and without git's `a/` prefix; empty when
    /// the header named none.
    pub old_path: Vec<u8>,
    /// The new path, as `old_path`.
    pub new_path: Vec<u8>,
    /// The old side's octal mode, when a header line gave one.
    pub old_mode: Option<String>,
    /// The new side's octal mode, when a header line gave one.
    pub new_mode: Option<String>,
    /// A rename's, copy's or rewrite's similarity index, 0 to 100.
    pub similarity: Option<u8>,
    /// `Binary files ... differ` or a `GIT binary patch`: no hunks follow.
    pub binary: bool,
}

impl FileInfo {
    fn new(old_path: Vec<u8>, new_path: Vec<u8>) -> Self {
        Self {
            status: FileStatus::Modified,
            old_path,
            new_path,
            old_mode: None,
            new_mode: None,
            similarity: None,
            binary: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Outside any file: commit headers, messages, other text.
    Text,
    /// Inside a file, before or between hunks.
    File,
    Hunk {
        old: usize,
        new: usize,
    },
    Combined {
        parents: usize,
    },
    /// A `GIT binary patch` body, skipped.
    BinaryPatch,
}

/// Turns the lines of a patch into [`Event`]s as they're fed.
pub struct Parser {
    state: State,
    file: FileInfo,
    /// The file header hasn't been sent yet.
    file_pending: bool,
    /// The previous line was `--- <path>` outside a file; it starts a file
    /// if `+++ ` follows.
    pending_minus: Option<Vec<u8>>,
    /// Lines since a `commit` header are metadata until the first blank.
    in_commit_meta: bool,
    /// `diff --git` strips `a/` and `b/` prefixes; plain `diff -u` doesn't.
    git_prefixes: bool,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            state: State::Text,
            file: FileInfo::new(Vec::new(), Vec::new()),
            file_pending: false,
            pending_minus: None,
            in_commit_meta: false,
            git_prefixes: false,
        }
    }
}

impl Parser {
    /// Parses one line, without its newline and with escape sequences
    /// already removed (see [`strip_ansi`]), passing what it finds to
    /// `emit`. A line may produce no event (a header line the file's event
    /// carries later) or several.
    ///
    /// A carriage return before the newline is dropped from header and
    /// text lines, so a patch saved with CRLF line ends reads the same; in
    /// hunk lines it's content and stays.
    ///
    /// # Errors
    ///
    /// The first error `emit` returns, which stops the line there.
    pub fn feed<E>(
        &mut self,
        line: &[u8],
        emit: &mut impl FnMut(Event) -> Result<(), E>,
    ) -> Result<(), E> {
        match self.state {
            State::Hunk { old, new } => self.hunk_line(line, old, new, emit),
            State::File => self.file_line(line, emit),
            State::Combined { parents } => {
                if starts_file_or_commit(line) {
                    self.state = State::Text;
                    self.text_line(line, emit)
                } else {
                    let parents = if line.starts_with(b"@@@") {
                        line.iter().take_while(|&&b| b == b'@').count() - 1
                    } else {
                        parents
                    };
                    self.state = State::Combined { parents };
                    emit(Event::Combined { parents, line })
                }
            }
            State::BinaryPatch => {
                if starts_file_or_commit(line) {
                    self.state = State::Text;
                    self.text_line(line, emit)
                } else {
                    Ok(())
                }
            }
            State::Text => self.text_line(line, emit),
        }
    }

    /// Flushes whatever is pending at the end of the input: the open
    /// hunk's end, a file header without hunks, a held `--- ` line.
    ///
    /// # Errors
    ///
    /// The first error `emit` returns.
    pub fn finish<E>(&mut self, emit: &mut impl FnMut(Event) -> Result<(), E>) -> Result<(), E> {
        if let State::Hunk { .. } = self.state {
            emit(Event::HunkEnd)?;
        }
        self.send_file(emit)?;
        if let Some(minus) = self.pending_minus.take() {
            emit(Event::Text(TextKind::Plain, &minus))?;
        }
        self.state = State::Text;
        Ok(())
    }

    fn hunk_line<E>(
        &mut self,
        line: &[u8],
        mut old: usize,
        mut new: usize,
        emit: &mut impl FnMut(Event) -> Result<(), E>,
    ) -> Result<(), E> {
        let (kind, text) = match line.first() {
            Some(b' ') if old > 0 && new > 0 => (Kind::Context, &line[1..]),
            // Some tools strip the trailing space of empty context lines.
            None if old > 0 && new > 0 => (Kind::Context, line),
            Some(b'-') if old > 0 => (Kind::Removed, &line[1..]),
            Some(b'+') if new > 0 => (Kind::Added, &line[1..]),
            Some(b'\\') => return emit(Event::Line(Kind::NoNewline, line)),
            _ => {
                // The counts didn't hold; end the hunk and read the line
                // afresh.
                emit(Event::HunkEnd)?;
                self.state = State::File;
                return self.file_line(line, emit);
            }
        };
        match kind {
            Kind::Context => {
                old -= 1;
                new -= 1;
            }
            Kind::Removed => old -= 1,
            Kind::Added => new -= 1,
            Kind::NoNewline => {}
        }
        emit(Event::Line(kind, text))?;
        if old == 0 && new == 0 {
            self.state = State::File;
            emit(Event::HunkEnd)
        } else {
            self.state = State::Hunk { old, new };
            Ok(())
        }
    }

    fn file_line<E>(
        &mut self,
        line: &[u8],
        emit: &mut impl FnMut(Event) -> Result<(), E>,
    ) -> Result<(), E> {
        // A patch saved with CRLF line ends; in hunk lines, a carriage
        // return is content.
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(hunk) = parse_hunk_header(line) {
            self.send_file(emit)?;
            emit(Event::Hunk(hunk))?;
            self.state = if hunk.old_len == 0 && hunk.new_len == 0 {
                emit(Event::HunkEnd)?;
                State::File
            } else {
                State::Hunk {
                    old: hunk.old_len,
                    new: hunk.new_len,
                }
            };
            return Ok(());
        }
        if line.starts_with(b"\\") {
            return emit(Event::Line(Kind::NoNewline, line));
        }
        if self.file_pending && self.header_line(line) {
            return Ok(());
        }
        // `--- ` may start the next file of `diff -u` output.
        if !self.file_pending
            && matches!(line.first(), Some(b'+' | b'-' | b' '))
            && !line.starts_with(b"--- ")
        {
            return emit(Event::Stray(line));
        }
        if line.starts_with(b"Binary files ") && line.ends_with(b" differ") {
            self.file.binary = true;
            return self.send_file(emit);
        }
        if line == b"GIT binary patch" {
            self.file.binary = true;
            self.send_file(emit)?;
            self.state = State::BinaryPatch;
            return Ok(());
        }
        self.send_file(emit)?;
        self.state = State::Text;
        self.text_line(line, emit)
    }

    /// Applies an extended header line (`rename from`, `new file mode`, ...)
    /// to the pending file. Returns false for lines that aren't headers.
    fn header_line(&mut self, line: &[u8]) -> bool {
        let f = &mut self.file;
        // Octal digits only: anything else on the line is never printed.
        let mode = |rest: &[u8]| {
            (!rest.is_empty() && rest.iter().all(u8::is_ascii_digit))
                .then(|| String::from_utf8_lossy(rest).into_owned())
        };
        if let Some(rest) = line.strip_prefix(b"--- ") {
            match strip_path(rest, self.git_prefixes) {
                None => f.status = FileStatus::Added,
                Some(path) => f.old_path = path,
            }
        } else if let Some(rest) = line.strip_prefix(b"+++ ") {
            match strip_path(rest, self.git_prefixes) {
                None => f.status = FileStatus::Deleted,
                Some(path) => f.new_path = path,
            }
        } else if let Some(rest) = line.strip_prefix(b"new file mode ") {
            f.status = FileStatus::Added;
            f.new_mode = mode(rest);
        } else if let Some(rest) = line.strip_prefix(b"deleted file mode ") {
            f.status = FileStatus::Deleted;
            f.old_mode = mode(rest);
        } else if let Some(rest) = line.strip_prefix(b"old mode ") {
            f.old_mode = mode(rest);
        } else if let Some(rest) = line.strip_prefix(b"new mode ") {
            f.new_mode = mode(rest);
        } else if let Some(rest) = line.strip_prefix(b"rename from ") {
            f.status = FileStatus::Renamed;
            f.old_path = unquote(rest);
        } else if let Some(rest) = line.strip_prefix(b"rename to ") {
            f.status = FileStatus::Renamed;
            f.new_path = unquote(rest);
        } else if let Some(rest) = line.strip_prefix(b"copy from ") {
            f.status = FileStatus::Copied;
            f.old_path = unquote(rest);
        } else if let Some(rest) = line.strip_prefix(b"copy to ") {
            f.status = FileStatus::Copied;
            f.new_path = unquote(rest);
        } else if let Some(rest) = line
            .strip_prefix(b"similarity index ")
            .or_else(|| line.strip_prefix(b"dissimilarity index "))
        {
            f.similarity = std::str::from_utf8(rest.strip_suffix(b"%").unwrap_or(rest))
                .ok()
                .and_then(|s| s.parse().ok());
        } else if let Some(rest) = line.strip_prefix(b"index ") {
            // `index abc..def 100644`: the mode of an unchanged-mode file.
            if let Some(space) = rest.iter().position(|&b| b == b' ')
                && f.old_mode.is_none()
                && f.new_mode.is_none()
            {
                f.old_mode = mode(&rest[space + 1..]);
                f.new_mode = f.old_mode.clone();
            }
        } else {
            return false;
        }
        true
    }

    fn text_line<E>(
        &mut self,
        line: &[u8],
        emit: &mut impl FnMut(Event) -> Result<(), E>,
    ) -> Result<(), E> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(minus) = self.pending_minus.take() {
            if let Some(rest) = line.strip_prefix(b"+++ ") {
                // `diff -u` output: a file without a `diff --git` line.
                let old = minus.strip_prefix(b"--- ").unwrap_or(&minus);
                self.start_file(Vec::new(), Vec::new(), false);
                self.header_line(&[b"--- ", old].concat());
                self.header_line(&[b"+++ ", rest].concat());
                return Ok(());
            }
            emit(Event::Text(TextKind::Plain, &minus))?;
        }
        if let Some(rest) = line.strip_prefix(b"diff --git ") {
            let (old, new) = git_header_paths(rest);
            self.start_file(old, new, true);
            return Ok(());
        }
        if let Some(rest) = line
            .strip_prefix(b"diff --cc ")
            .or_else(|| line.strip_prefix(b"diff --combined "))
        {
            let path = unquote(rest);
            self.start_file(path.clone(), path, true);
            self.file.status = FileStatus::Combined;
            self.send_file(emit)?;
            self.state = State::Combined { parents: 0 };
            return Ok(());
        }
        if line.starts_with(b"diff ") {
            return emit(Event::Text(TextKind::Meta, line));
        }
        if line.starts_with(b"--- ") {
            self.pending_minus = Some(line.to_vec());
            return Ok(());
        }
        let kind = if is_commit_header(line) {
            self.in_commit_meta = true;
            TextKind::CommitHeader
        } else if line.is_empty() {
            self.in_commit_meta = false;
            TextKind::Plain
        } else if self.in_commit_meta && is_meta_line(line) {
            TextKind::CommitMeta
        } else if line.starts_with(b"    ") {
            TextKind::CommitMessage
        } else {
            TextKind::Plain
        };
        emit(Event::Text(kind, line))
    }

    fn start_file(&mut self, old: Vec<u8>, new: Vec<u8>, git: bool) {
        self.file = FileInfo::new(old, new);
        self.file_pending = true;
        self.git_prefixes = git;
        self.in_commit_meta = false;
        self.state = State::File;
    }

    /// Sends the file header if it hasn't gone out yet. Files without hunks
    /// (mode changes, pure renames, empty new files) send it when the next
    /// file or text begins.
    fn send_file<E>(&mut self, emit: &mut impl FnMut(Event) -> Result<(), E>) -> Result<(), E> {
        if self.file_pending {
            self.file_pending = false;
            emit(Event::File(&self.file))?;
        }
        Ok(())
    }
}

fn starts_file_or_commit(line: &[u8]) -> bool {
    line.starts_with(b"diff ") || is_commit_header(line)
}

/// Whether `line` is `commit <hex id>`, optionally followed by
/// decorations, with an id of at least 7 digits.
#[inline]
#[must_use]
pub fn is_commit_header(line: &[u8]) -> bool {
    let Some(rest) = line.strip_prefix(b"commit ") else {
        return false;
    };
    let id_len = rest.iter().take_while(|b| b.is_ascii_hexdigit()).count();
    id_len >= 7 && (rest.len() == id_len || rest[id_len] == b' ')
}

/// Whether `line` is `Name: value` with a capitalized name, as in
/// `Author:` or `CommitDate:`.
#[inline]
#[must_use]
pub fn is_meta_line(line: &[u8]) -> bool {
    let name_len = line.iter().take_while(|b| b.is_ascii_alphabetic()).count();
    name_len > 0 && line[0].is_ascii_uppercase() && line.get(name_len) == Some(&b':')
}

/// Parses `@@ -a[,b] +c[,d] @@[ heading]`. `None` for anything else,
/// including a combined diff's `@@@` and a range ending past the largest
/// `usize`.
#[must_use]
pub fn parse_hunk_header(line: &[u8]) -> Option<Hunk<'_>> {
    let rest = line.strip_prefix(b"@@ -")?;
    let end = rest.windows(3).position(|w| w == b" @@")?;
    let (ranges, tail) = (&rest[..end], &rest[end + 3..]);
    let ranges = std::str::from_utf8(ranges).ok()?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |s: &str| -> Option<(usize, usize)> {
        match s.split_once(',') {
            Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (old_start, old_len) = range(old)?;
    let (new_start, new_len) = range(new)?;
    // Every reader adds start and length; a hunk ending past the largest
    // number isn't one.
    if old_start.checked_add(old_len).is_none() || new_start.checked_add(new_len).is_none() {
        return None;
    }
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
        heading: tail.strip_prefix(b" ").unwrap_or(tail),
    })
}

/// The path of a `---`/`+++` line, or `None` for `/dev/null`. Drops a
/// trailing tab and timestamp (`diff -u`) and, for git, the one-letter
/// prefix (`a/`, or a mnemonic one like `i/` or `w/`).
fn strip_path(rest: &[u8], git: bool) -> Option<Vec<u8>> {
    let path = if rest.first() == Some(&b'"') {
        unquote(rest)
    } else {
        let end = rest.iter().position(|&b| b == b'\t').unwrap_or(rest.len());
        rest[..end].to_vec()
    };
    if path == b"/dev/null" {
        return None;
    }
    Some(if git { strip_git_prefix(path) } else { path })
}

fn strip_git_prefix(path: Vec<u8>) -> Vec<u8> {
    if path.len() > 2 && path[1] == b'/' && path[0].is_ascii_lowercase() {
        path[2..].to_vec()
    } else {
        path
    }
}

/// Paths from `diff --git a/x b/y`. Ambiguous with spaces, so `---`, `+++`
/// and rename lines take precedence when present.
fn git_header_paths(rest: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let strip = strip_git_prefix;
    if rest.first() == Some(&b'"') {
        let mut parts = split_quoted(rest);
        let new = parts.pop().unwrap_or_default();
        let old = parts.pop().unwrap_or_default();
        return (strip(old), strip(new));
    }
    // Equal halves (`a/x b/x`) are the common, unambiguous case.
    if rest.len() % 2 == 1 {
        let half = rest.len() / 2;
        if rest[half] == b' ' && rest.get(2..half) == rest.get(half + 3..) {
            return (
                strip(rest[..half].to_vec()),
                strip(rest[half + 1..].to_vec()),
            );
        }
    }
    match rest.windows(3).rposition(|w| w == b" b/") {
        Some(i) => (strip(rest[..i].to_vec()), strip(rest[i + 1..].to_vec())),
        None => (rest.to_vec(), rest.to_vec()),
    }
}

/// Splits space-separated, possibly C-quoted, words.
fn split_quoted(s: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        if s[i] == b' ' {
            i += 1;
        } else if s[i] == b'"' {
            let mut j = i + 1;
            while j < s.len() && s[j] != b'"' {
                j += if s[j] == b'\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(s.len());
            out.push(unquote(&s[i..end]));
            i = end;
        } else {
            let j = s[i..]
                .iter()
                .position(|&b| b == b' ')
                .map_or(s.len(), |p| i + p);
            out.push(s[i..j].to_vec());
            i = j;
        }
    }
    out
}

/// Undoes git's C-style path quoting (`"a/t\303\251st\tx"`); other input is
/// returned as is.
fn unquote(s: &[u8]) -> Vec<u8> {
    let Some(inner) = s.strip_prefix(b"\"").and_then(|s| s.strip_suffix(b"\"")) else {
        return s.to_vec();
    };
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        if inner[i] != b'\\' || i + 1 == inner.len() {
            out.push(inner[i]);
            i += 1;
            continue;
        }
        let c = inner[i + 1];
        i += 2;
        match c {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'v' => out.push(0x0b),
            b'0'..=b'7' => {
                let digits = inner[i - 1..]
                    .iter()
                    .take(3)
                    .take_while(|b| (b'0'..=b'7').contains(b))
                    .count();
                let value = inner[i - 1..i - 1 + digits]
                    .iter()
                    .fold(0u32, |v, &d| v * 8 + u32::from(d - b'0'));
                out.push(u8::try_from(value).unwrap_or(b'?'));
                i += digits - 1;
            }
            other => out.push(other),
        }
    }
    out
}

/// Copies `line` into `out` without its escape sequences (SGR colors, OSC
/// hyperlinks, others), clearing `out` first. A sequence cut off by the
/// end of the line is dropped.
pub fn strip_ansi(line: &[u8], out: &mut Vec<u8>) {
    out.clear();
    let mut i = 0;
    while i < line.len() {
        let Some(esc) = line[i..].iter().position(|&b| b == 0x1b) else {
            out.extend_from_slice(&line[i..]);
            break;
        };
        out.extend_from_slice(&line[i..i + esc]);
        i += esc + 1;
        match line.get(i) {
            Some(b'[') => {
                i += 1;
                while i < line.len() && !(0x40..=0x7e).contains(&line[i]) {
                    i += 1;
                }
                i += 1;
            }
            Some(b']') => {
                i += 1;
                while i < line.len() {
                    if line[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if line[i] == 0x1b && line.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            Some(_) => i += 1,
            None => {}
        }
    }
}

/// What a line of a combined diff (`diff --cc`) is, by its prefix columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CombinedKind {
    /// A header line before the first hunk: `index`, `---`, `+++`, modes.
    Header,
    /// A hunk's `@@@ ... @@@` line.
    HunkHeader,
    /// Not in the result: some column is `-`.
    Removed,
    /// In the result and not in some parent: a column is `+`, none `-`.
    Added,
    /// In the result and every parent.
    Context,
}

/// What `line` of a combined diff is, given its `parents` prefix columns
/// (see [`Event::Combined`]).
#[inline]
#[must_use]
pub fn combined_kind(parents: usize, line: &[u8]) -> CombinedKind {
    if line.starts_with(b"@@@") {
        CombinedKind::HunkHeader
    } else if parents == 0 {
        CombinedKind::Header
    } else {
        let prefix = &line[..parents.min(line.len())];
        if prefix.contains(&b'-') {
            CombinedKind::Removed
        } else if prefix.contains(&b'+') {
            CombinedKind::Added
        } else {
            CombinedKind::Context
        }
    }
}

/// A commit's header and message, or text that came between files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// The commit's full id; `None` for text that isn't a commit.
    pub id: Option<String>,
    /// The first non-blank message line, trimmed; empty for text that
    /// isn't a commit.
    pub subject: String,
    /// The lines as they came, without trailing blank ones.
    pub lines: Vec<(TextKind, Vec<u8>)>,
}

impl Commit {
    /// The commit `id` with `lines`, as `git show` starts with them: its
    /// header, metadata and message. The subject is read from the lines.
    #[must_use]
    pub fn new(id: String, lines: Vec<(TextKind, Vec<u8>)>) -> Self {
        let subject = lines
            .iter()
            .find(|(kind, text)| {
                matches!(kind, TextKind::Plain | TextKind::CommitMessage) && !trim(text).is_empty()
            })
            .map(|(_, text)| String::from_utf8_lossy(trim(text)).into_owned())
            .unwrap_or_default();
        Self {
            id: Some(id),
            subject,
            lines,
        }
    }

    /// A one-line name: the short id and the subject, or the text's first
    /// line, trimmed.
    #[must_use]
    pub fn label(&self) -> Vec<u8> {
        match &self.id {
            Some(id) => format!("{} {}", &id[..id.len().min(7)], self.subject).into_bytes(),
            None => self
                .lines
                .first()
                .map(|(_, text)| trim(text).to_vec())
                .unwrap_or_default(),
        }
    }
}

/// A file of a patch, as [`Builder`] completes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    /// The file's header.
    pub info: FileInfo,
    /// Added lines in its hunks; for a combined diff, lines with a `+`
    /// column.
    pub added: usize,
    /// Removed lines in its hunks; for a combined diff, lines with a `-`
    /// column and no `+`.
    pub removed: usize,
    /// A combined diff's lines (`diff --cc`), each whole with its number
    /// of prefix columns (see [`Event::Combined`]); `None` for any other
    /// file, which has its hunks in `hunks`.
    pub combined: Option<Vec<(usize, Vec<u8>)>>,
    /// The hunks as unified diff text without colors: each `@@` line, then
    /// its lines with their prefixes and markers, every line ending in a
    /// newline. [`sides`] reads it back.
    pub hunks: Vec<u8>,
}

/// A commit or a file of a patch, in input order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// A commit's header and message, or text between files.
    Commit(Commit),
    /// A file and its hunks.
    File(File),
}

/// Lines of text with what each one is.
type Lines = Vec<(TextKind, Vec<u8>)>;

/// Collects a [`Parser`]'s events into [`Item`]s. Each item goes out once
/// the next one starts (or at [`Builder::finish`]), so a host holds at most
/// one commit's lines and one file's hunks at a time.
#[derive(Debug, Default)]
pub struct Builder {
    /// A commit's id (`None` for a text block) and its lines.
    pending: Option<(Option<String>, Lines)>,
    file: Option<File>,
}

impl Builder {
    /// Takes in one event, passing any item it completes to `emit`. Blank
    /// lines outside a commit start no text block of their own; a stray
    /// line changes nothing.
    ///
    /// # Errors
    ///
    /// The error `emit` returns.
    pub fn event<E>(
        &mut self,
        event: Event,
        emit: &mut impl FnMut(Item) -> Result<(), E>,
    ) -> Result<(), E> {
        match event {
            Event::Text(kind, line) => {
                self.send_file(emit)?;
                if kind == TextKind::CommitHeader {
                    self.send_pending(emit)?;
                    let id = line[b"commit ".len()..]
                        .iter()
                        .take_while(|b| b.is_ascii_hexdigit())
                        .map(|&b| char::from(b))
                        .collect();
                    self.pending = Some((Some(id), Vec::new()));
                }
                if self.pending.is_none() && trim(line).is_empty() {
                    return Ok(());
                }
                let (_, lines) = self.pending.get_or_insert_with(|| (None, Vec::new()));
                lines.push((kind, line.to_vec()));
            }
            Event::File(info) => {
                self.send_file(emit)?;
                self.send_pending(emit)?;
                self.file = Some(File {
                    info: info.clone(),
                    added: 0,
                    removed: 0,
                    combined: (info.status == FileStatus::Combined).then(Vec::new),
                    hunks: Vec::new(),
                });
            }
            Event::Hunk(h) => {
                if let Some(f) = &mut self.file {
                    let numbers = HunkHeader {
                        old_start: h.old_start,
                        old_len: h.old_len,
                        new_start: h.new_start,
                        new_len: h.new_len,
                        heading: h.heading,
                        hidden: 0,
                    }
                    .numbers();
                    f.hunks.extend_from_slice(numbers.as_bytes());
                    if !h.heading.is_empty() {
                        f.hunks.push(b' ');
                        f.hunks.extend_from_slice(h.heading);
                    }
                    f.hunks.push(b'\n');
                }
            }
            Event::Line(kind, text) => {
                if let Some(f) = &mut self.file {
                    let prefix = match kind {
                        Kind::Context => Some(b' '),
                        Kind::Removed => {
                            f.removed += 1;
                            Some(b'-')
                        }
                        Kind::Added => {
                            f.added += 1;
                            Some(b'+')
                        }
                        // The whole marker line.
                        Kind::NoNewline => None,
                    };
                    f.hunks.extend(prefix);
                    f.hunks.extend_from_slice(text);
                    f.hunks.push(b'\n');
                }
            }
            // The hunks are read back by their counts, which a stray line
            // isn't part of.
            Event::HunkEnd | Event::Stray(_) => {}
            Event::Combined { parents, line } => {
                if let Some(f) = &mut self.file {
                    let prefix = &line[..parents.min(line.len())];
                    if !line.starts_with(b"@@@") && parents > 0 {
                        f.added += usize::from(prefix.contains(&b'+'));
                        f.removed += usize::from(prefix.contains(&b'-') && !prefix.contains(&b'+'));
                    }
                    f.combined
                        .get_or_insert_with(Vec::new)
                        .push((parents, line.to_vec()));
                }
            }
        }
        Ok(())
    }

    /// Passes the last file or commit to `emit`, at the end of the input
    /// and after [`Parser::finish`].
    ///
    /// # Errors
    ///
    /// The first error `emit` returns.
    pub fn finish<E>(&mut self, emit: &mut impl FnMut(Item) -> Result<(), E>) -> Result<(), E> {
        self.send_file(emit)?;
        self.send_pending(emit)
    }

    fn send_pending<E>(&mut self, emit: &mut impl FnMut(Item) -> Result<(), E>) -> Result<(), E> {
        let Some((id, mut lines)) = self.pending.take() else {
            return Ok(());
        };
        while lines.last().is_some_and(|(_, l)| trim(l).is_empty()) {
            lines.pop();
        }
        let commit = match id {
            Some(id) => Commit::new(id, lines),
            None if lines.is_empty() => return Ok(()),
            None => Commit {
                id: None,
                subject: String::new(),
                lines,
            },
        };
        emit(Item::Commit(commit))
    }

    fn send_file<E>(&mut self, emit: &mut impl FnMut(Item) -> Result<(), E>) -> Result<(), E> {
        match self.file.take() {
            Some(file) => emit(Item::File(file)),
            None => Ok(()),
        }
    }
}

/// The line a file's hunks may start at, at most, for [`sides`]. A file is
/// laid out over its real line numbers, at about 9 bytes per line before a
/// hunk, so this caps one file at about 150MB.
pub const MAX_LINES: usize = 1 << 24;

/// A hunk starts past [`MAX_LINES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooFar;

impl fmt::Display for TooFar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a hunk starts past line {MAX_LINES}, more than a piped file can hold"
        )
    }
}

impl std::error::Error for TooFar {}

/// A file's sides as far as its hunks show them (see [`sides`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sides {
    /// The old side.
    pub old: Text,
    /// The new side.
    pub new: Text,
    /// The hunks, numbered by lines of `old` and `new`, with no emphasis
    /// and nothing paired.
    pub hunks: Vec<diff::Hunk>,
    /// Each hunk's heading as the patch had it.
    pub headings: Vec<Vec<u8>>,
}

/// Lays a file's [`File::hunks`] out over its real line numbers: each
/// side's text holds the lines the hunks show, at their place, and an empty
/// line for each line the patch left out. A no-newline marker takes the
/// newline off the line before it, on that line's side or sides.
///
/// # Errors
///
/// [`TooFar`] when a hunk starts past [`MAX_LINES`], as a corrupt or
/// hostile patch can claim, instead of filling memory with the lines
/// before it.
pub fn sides(hunks: &[u8]) -> Result<Sides, TooFar> {
    let (mut old, mut new) = (Vec::new(), Vec::new());
    let (mut old_n, mut new_n) = (0, 0);
    let mut out = Vec::new();
    let mut headings = Vec::new();
    // The side or sides of the last line, for a no-newline marker.
    let mut last = None;
    // Split on newlines alone: a carriage return is content.
    for line in hunks
        .strip_suffix(b"\n")
        .unwrap_or(hunks)
        .split(|&b| b == b'\n')
    {
        if let Some(h) = parse_hunk_header(line) {
            let zero = |start: usize, len: usize| {
                if len == 0 {
                    start
                } else {
                    start.saturating_sub(1)
                }
            };
            let (os, ns) = (zero(h.old_start, h.old_len), zero(h.new_start, h.new_len));
            if os.max(ns) > MAX_LINES {
                return Err(TooFar);
            }
            // A byte and a line start per line left out: a hunk at line
            // 1,000,000 costs 9MB while the file is loaded. A sparse text
            // is the upgrade if that shows up.
            old.resize(old.len() + os.saturating_sub(old_n), b'\n');
            new.resize(new.len() + ns.saturating_sub(new_n), b'\n');
            (old_n, new_n) = (old_n.max(os), new_n.max(ns));
            out.push(diff::Hunk {
                old_start: old_n,
                old_len: h.old_len,
                new_start: new_n,
                new_len: h.new_len,
                lines: Vec::new(),
            });
            headings.push(h.heading.to_vec());
            last = None;
            continue;
        }
        let Some(hunk) = out.last_mut() else {
            continue;
        };
        let (kind, text) = match line.split_first() {
            Some((b'-', text)) => (LineKind::Removed, text),
            Some((b'+', text)) => (LineKind::Added, text),
            Some((b'\\', _)) => {
                // The line before has no newline on its side(s).
                if matches!(last, Some(LineKind::Removed | LineKind::Context)) {
                    old.pop();
                }
                if matches!(last, Some(LineKind::Added | LineKind::Context)) {
                    new.pop();
                }
                last = None;
                continue;
            }
            Some((_, text)) => (LineKind::Context, text),
            None => (LineKind::Context, line),
        };
        hunk.lines.push(diff::Line {
            kind,
            old: old_n,
            new: new_n,
            emphasis: Vec::new(),
            unchanged: Unchanged::No,
            block_start: false,
            paired: false,
        });
        if kind != LineKind::Added {
            old.extend_from_slice(text);
            old.push(b'\n');
            old_n += 1;
        }
        if kind != LineKind::Removed {
            new.extend_from_slice(text);
            new.push(b'\n');
            new_n += 1;
        }
        last = Some(kind);
    }
    Ok(Sides {
        old: Text::new(old),
        new: Text::new(new),
        hunks: out,
        headings,
    })
}

/// `text` without leading and trailing Unicode whitespace. Bytes that
/// aren't UTF-8 are never whitespace.
fn trim(text: &[u8]) -> &[u8] {
    let start = text.utf8_chunks().next().map_or(0, |chunk| {
        chunk.valid().len() - chunk.valid().trim_start().len()
    });
    let end = text.utf8_chunks().last().map_or(0, |chunk| {
        let trailing = if chunk.invalid().is_empty() {
            chunk.valid().len() - chunk.valid().trim_end().len()
        } else {
            0
        };
        text.len() - trailing
    });
    &text[start..end.max(start)]
}

#[cfg(test)]
mod tests;

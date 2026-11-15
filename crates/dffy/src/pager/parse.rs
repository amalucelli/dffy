//! A line-at-a-time parser for unified diffs as `git diff`, `git show`,
//! `git log -p` and `diff -u` print them. It keeps only the current file's
//! header, so memory doesn't grow with the input.
//!
//! Hunk bodies are delimited by the line counts in their `@@` header, not by
//! what lines look like: a removed line reading `-- foo` is content, not a
//! file header.

use crate::render::{FileStatus, Kind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextKind {
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

#[derive(Debug, Clone, Copy)]
pub(crate) enum Event<'a> {
    Text(TextKind, &'a [u8]),
    /// A file's header is complete; sent before its first hunk.
    File(&'a FileInfo),
    Hunk(Hunk<'a>),
    /// A hunk line without its prefix, or the whole `\ No newline` marker.
    /// A marker may also follow a finished hunk.
    Line(Kind, &'a [u8]),
    HunkEnd,
    /// A whole `+`, `-` or ` ` line inside a file but past its hunk's
    /// counts. It belongs to no hunk, so it's no change; the file goes on,
    /// and the next `@@` starts its next hunk.
    Stray(&'a [u8]),
    /// A line of a combined diff (`diff --cc`), passed through whole.
    /// `parents` is the number of prefix columns, 0 for the header lines
    /// before the first `@@@`.
    Combined {
        parents: usize,
        line: &'a [u8],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Hunk<'a> {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub heading: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileInfo {
    pub status: FileStatus,
    pub old_path: Vec<u8>,
    pub new_path: Vec<u8>,
    pub old_mode: Option<String>,
    pub new_mode: Option<String>,
    pub similarity: Option<u8>,
    pub binary: bool,
}

impl FileInfo {
    pub(crate) fn new(old_path: Vec<u8>, new_path: Vec<u8>) -> Self {
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

pub(crate) struct Parser {
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
    /// already removed.
    pub(crate) fn feed<E>(
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

    /// Flushes whatever is pending at the end of the input.
    pub(crate) fn finish<E>(
        &mut self,
        emit: &mut impl FnMut(Event) -> Result<(), E>,
    ) -> Result<(), E> {
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

/// `commit <hex id>`, optionally followed by decorations.
pub(crate) fn is_commit_header(line: &[u8]) -> bool {
    let Some(rest) = line.strip_prefix(b"commit ") else {
        return false;
    };
    let id_len = rest.iter().take_while(|b| b.is_ascii_hexdigit()).count();
    id_len >= 7 && (rest.len() == id_len || rest[id_len] == b' ')
}

/// `Name: value` with a capitalized name, as in `Author:` or `CommitDate:`.
pub(crate) fn is_meta_line(line: &[u8]) -> bool {
    let name_len = line.iter().take_while(|b| b.is_ascii_alphabetic()).count();
    name_len > 0 && line[0].is_ascii_uppercase() && line.get(name_len) == Some(&b':')
}

/// Parses `@@ -a[,b] +c[,d] @@[ heading]`.
pub(crate) fn parse_hunk_header(line: &[u8]) -> Option<Hunk<'_>> {
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

/// Removes escape sequences (SGR colors, OSC hyperlinks, others) from
/// `line` into `out`.
pub(crate) fn strip_ansi(line: &[u8], out: &mut Vec<u8>) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_sgr_and_osc_sequences() {
        let mut out = Vec::new();
        strip_ansi(
            b"\x1b[1mdiff --git\x1b[m a\x1b]8;;http://x\x07link\x1b]8;;\x1b\\",
            &mut out,
        );
        assert_eq!(out, b"diff --git alink");
        strip_ansi(b"\x1b[32m+\x1b[m\x1b[32madded\x1b[m", &mut out);
        assert_eq!(out, b"+added");
        strip_ansi(b"trailing\x1b", &mut out);
        assert_eq!(out, b"trailing");
    }

    #[test]
    fn hunk_headers_default_lengths_to_one() {
        let h = parse_hunk_header(b"@@ -3 +4,0 @@ fn main()").expect("valid");
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (3, 1, 4, 0)
        );
        assert_eq!(h.heading, b"fn main()");
        assert!(parse_hunk_header(b"@@ -x +1 @@").is_none());
        assert!(parse_hunk_header(b"@@@ -1,2 -1,2 +1,3 @@@").is_none());
        // Ends past the largest line number.
        assert!(parse_hunk_header(b"@@ -18446744073709551615 +1 @@").is_none());
        assert!(parse_hunk_header(b"@@ -5 +5,18446744073709551615 @@").is_none());
    }

    #[test]
    fn modes_are_octal_digits_only() {
        let mut parser = Parser::default();
        let mut modes = None;
        for line in [
            &b"diff --git a/x b/x"[..],
            b"old mode 100644\x07\xc2\x9b2J",
            b"new mode 100755",
        ] {
            parser
                .feed(line, &mut |e| {
                    if let Event::File(f) = e {
                        modes = Some((f.old_mode.clone(), f.new_mode.clone()));
                    }
                    Ok::<(), ()>(())
                })
                .expect("feeds");
        }
        parser
            .finish(&mut |e| {
                if let Event::File(f) = e {
                    modes = Some((f.old_mode.clone(), f.new_mode.clone()));
                }
                Ok::<(), ()>(())
            })
            .expect("finishes");
        assert_eq!(modes, Some((None, Some("100755".to_owned()))));
    }

    /// The events of `lines`, as text: `file <old> <new>`, `hunk <heading>`,
    /// `+`/`-`/` ` lines, `end`, `stray <line>`, `text <line>`.
    fn events(lines: &[&[u8]]) -> Vec<String> {
        let mut parser = Parser::default();
        let mut out = Vec::new();
        let mut take = |e: Event| {
            let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
            out.push(match e {
                Event::File(f) => format!("file {} {}", s(&f.old_path), s(&f.new_path)),
                Event::Hunk(h) => format!("hunk {}", s(h.heading)),
                Event::Line(Kind::Added, t) => format!("+{}", s(t)),
                Event::Line(Kind::Removed, t) => format!("-{}", s(t)),
                Event::Line(_, t) => format!(" {}", s(t)),
                Event::HunkEnd => "end".to_owned(),
                Event::Stray(t) => format!("stray {}", s(t)),
                Event::Text(_, t) | Event::Combined { line: t, .. } => {
                    format!("text {}", s(t))
                }
            });
            Ok::<(), ()>(())
        };
        for line in lines {
            parser.feed(line, &mut take).expect("feeds");
        }
        parser.finish(&mut take).expect("finishes");
        out
    }

    /// A line past a hunk's counts ends the hunk and shows as no change;
    /// the file's next hunk is still read as one.
    #[test]
    fn a_stray_line_past_the_counts_resyncs_at_the_next_hunk() {
        let got = events(&[
            b"diff --git a/x b/x",
            b"--- a/x",
            b"+++ b/x",
            b"@@ -1 +1 @@",
            b"-a",
            b"+b",
            b"+stray",
            b"@@ -5 +5 @@ f",
            b"-c",
            b"+d",
        ]);
        assert_eq!(
            got,
            [
                "file x x",
                "hunk ",
                "-a",
                "+b",
                "end",
                "stray +stray",
                "hunk f",
                "-c",
                "+d",
                "end",
            ]
        );
    }

    /// A patch saved with CRLF line ends: headers lose the carriage return,
    /// hunk lines keep it as content.
    #[test]
    fn crlf_headers_are_read_without_the_carriage_return() {
        let got = events(&[
            b"diff --git a/x b/x\r",
            b"index 0123456..789abcd 100644\r",
            b"--- a/x\r",
            b"+++ b/x\r",
            b"@@ -1 +1 @@ f\r",
            b"-a\r",
            b"+b\r",
        ]);
        assert_eq!(got, ["file x x", "hunk f", "-a\r", "+b\r", "end"]);
    }

    #[test]
    fn git_header_paths_handle_spaces_and_quotes() {
        assert_eq!(
            git_header_paths(b"a/my file.c b/my file.c"),
            (b"my file.c".to_vec(), b"my file.c".to_vec())
        );
        assert_eq!(
            git_header_paths(b"a/old.c b/new name.c"),
            (b"old.c".to_vec(), b"new name.c".to_vec())
        );
        assert_eq!(
            git_header_paths(b"\"a/t\\303\\251st\\tx\" \"b/t\\303\\251st\\tx\""),
            ("tést\tx".as_bytes().to_vec(), "tést\tx".as_bytes().to_vec())
        );
    }

    #[test]
    fn commit_headers_need_a_hex_id() {
        assert!(is_commit_header(b"commit 0123abc"));
        assert!(is_commit_header(b"commit 0123abcdef (HEAD -> main)"));
        assert!(!is_commit_header(b"commit message"));
        assert!(is_meta_line(b"Author: A <a@b>"));
        assert!(!is_meta_line(b"    Signed-off-by: x"));
    }
}

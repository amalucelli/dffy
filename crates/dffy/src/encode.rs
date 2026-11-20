//! The core model as JSON: the header, file objects with their hunks and
//! lines, annotations with their threads, and where a thread is placed, in
//! the format docs/json.md specifies. `--json`, MCP and the control API
//! wrap these in envelopes of their own (JSON Lines, tool results, protocol
//! answers) but never spell the objects themselves, so their schemas can't
//! drift. A change that would break a consumer bumps [`VERSION`].

use std::fmt::Write as _;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use dffy_core::cache::Cache;
use dffy_core::diff::{self, Hunk, LineKind, Options, Text, Unchanged, Whitespace};
use dffy_core::document::{self, File, Side};
use dffy_core::git::{DiffSpec, FileChange, FileDiff, MAX_DIFF_BYTES, Repo, StatStore, Status};
use dffy_core::highlight::{HunkSyntax, Registry, Source};
use dffy_core::structural::{Structural, Unavailable};
use dffy_core::view::{Heading, Spans, line_bytes};

use crate::config::Context;
use crate::plugin::json::{Value, string};
use crate::print::{FileOutput, Plan};
use crate::review::model::At;
use crate::review::model::snapshot::{Hung, Snapshot};
use crate::{Error, review};

/// The schema's version, in the header.
pub(crate) const VERSION: u32 = 1;

/// A file object goes to the writer in parts of about this size, so a large
/// file isn't held whole.
const PART_BYTES: usize = 64 << 10;

/// What was compared, as given on the command line.
pub(crate) struct Compared<'a> {
    /// Revisions as typed.
    pub revisions: &'a [String],
    /// The two files or folders compared outside git, as given.
    pub operands: Option<&'a [PathBuf; 2]>,
}

/// What the file objects hold beyond their fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detail {
    /// Hunks and lines.
    Hunks,
    /// Hunks and lines, each line with its highlights.
    Highlights,
    /// No hunks: `--stat`, `--name-only` or `--name-status`.
    Summary,
}

/// The header object, without a line break: `context` and `renames` are
/// the settings `files` were made with.
pub(crate) fn header(
    plan: &Plan,
    compared: &Compared,
    context: Context,
    renames: bool,
    files: &Files,
) -> String {
    let mut s = String::new();
    let tree = |id| format!(r#"{{"kind":"tree","id":"{id}"}}"#);
    let index = || r#"{"kind":"index"}"#.to_owned();
    let worktree = || r#"{"kind":"worktree"}"#.to_owned();
    let (mut old, mut new, untracked) = match plan.spec {
        DiffSpec::Uncommitted { head } => (tree(head), worktree(), true),
        DiffSpec::IndexToWorktree { untracked } => (index(), worktree(), untracked),
        DiffSpec::TreeToIndex { tree: id } => (tree(id), index(), false),
        DiffSpec::TreeToWorktree { tree: id } => (tree(id), worktree(), false),
        DiffSpec::TreeToTree { old, new } => (tree(old), tree(new), false),
        DiffSpec::Paths => {
            let path = |i: usize| {
                let mut side = r#"{"kind":"path","path":"#.to_owned();
                let given = compared.operands.map(|o| o[i].to_string_lossy());
                string(&mut side, given.as_deref().unwrap_or_default());
                side.push('}');
                side
            };
            (path(0), path(1), false)
        }
    };
    if plan.reverse {
        std::mem::swap(&mut old, &mut new);
    }
    let _ = write!(
        s,
        r#"{{"type":"header","version":{VERSION},"old":{old},"new":{new},"untracked":{untracked},"revisions":["#
    );
    for (i, rev) in compared.revisions.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        string(&mut s, rev);
    }
    s.push_str(r#"],"paths":["#);
    for (i, path) in plan.paths.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        string(&mut s, &String::from_utf8_lossy(path));
    }
    s.push_str(r#"],"options":{"context":"#);
    match context {
        Context::Lines(n) => {
            let _ = write!(s, "{n}");
        }
        Context::Full => s.push_str(r#""full""#),
    }
    let whitespace = match files.options.whitespace {
        Whitespace::Exact => "exact",
        Whitespace::IgnoreAll => "ignore_all_space",
        Whitespace::IgnoreChange => "ignore_space_change",
    };
    let _ = write!(
        s,
        r#","whitespace":"{whitespace}","syntax":{},"renames":{renames},"highlights":{},"summary":{}}}}}"#,
        files.syntax_diff, files.highlights, files.summary,
    );
    s
}

/// Counts over the files written, for the end object and the exit code.
#[derive(Default)]
struct Totals {
    files: AtomicUsize,
    added: AtomicUsize,
    removed: AtomicUsize,
    differences: AtomicUsize,
}

/// The file objects: one per file, as a line of JSON.
pub(crate) struct Files<'a> {
    options: Options,
    syntax_diff: bool,
    highlights: bool,
    /// File objects without hunks.
    summary: bool,
    grammars: &'a Registry,
    cache: Option<&'a Cache>,
    /// A summary's counts, found in and kept in the disk cache; written
    /// when the files are dropped.
    stats: Option<StatStore>,
    totals: Totals,
    /// The threads each file object with hunks has as its `annotations`,
    /// and whether the sides are swapped (`-R`).
    pub comments: Option<(&'a Snapshot, bool)>,
}

/// The part of a file's hunks a file object holds, for a host that pages
/// through long diffs: from line `line` of hunk `hunk`, at most `max_lines`
/// lines and, past the first line, `max_bytes` bytes of their text. A hunk
/// that fits in a window of its own isn't split across two; a hunk cut
/// short gets the numbers of the lines it holds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Window {
    pub hunk: usize,
    pub line: usize,
    pub max_lines: usize,
    pub max_bytes: usize,
}

/// A file object written through a [`Window`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Windowed {
    /// `None` when the object holds the rest of the file.
    pub cut: Option<Cut>,
    /// A digest of both sides' text, so a later window can tell the file
    /// changed in between.
    pub digest: u64,
}

/// Where a [`Window`] stopped short of a file's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cut {
    /// Where the next window starts: a hunk, and a line in it.
    pub at: (usize, usize),
    /// Lines from there to the end.
    pub remaining: usize,
}

impl<'a> Files<'a> {
    /// File objects with the hunks of `options`, the syntax diff when
    /// `syntax_diff` is on, and what `detail` asks for.
    pub(crate) fn new(
        options: Options,
        syntax_diff: bool,
        detail: Detail,
        cache: Option<&'a Cache>,
    ) -> Self {
        let summary = detail == Detail::Summary;
        Self {
            options,
            syntax_diff,
            highlights: detail == Detail::Highlights,
            summary,
            grammars: crate::grammars(),
            cache,
            stats: cache
                .filter(|_| summary && !syntax_diff)
                .map(|c| StatStore::new(c.clone())),
            totals: Totals::default(),
            comments: None,
        }
    }

    /// A summary's file object that reads neither side: `added` and
    /// `removed` are null, `binary` and `too_large` false, and the
    /// language comes from the name alone.
    pub(crate) fn listed(&self, change: FileChange) -> String {
        let fd = FileDiff {
            change,
            old: Text::default(),
            new: Text::default(),
            binary: false,
            too_large: false,
            note: None,
            hunks: Vec::new(),
        };
        let file = FileFields {
            language: language(self.grammars, &fd),
            counts: None,
            syntax: None,
            classification: None,
        };
        let mut s = String::new();
        file_fields(&fd, &file, &mut s);
        s.push_str(r#","hunks":null}"#);
        s
    }

    /// `change`'s file object, without a line break, with its hunks limited
    /// to `window` when there is one. A summary's has no hunks to limit,
    /// so no [`Windowed`].
    pub(crate) fn file_object(
        &self,
        repo: &Repo,
        change: FileChange,
        window: Option<Window>,
    ) -> Result<(String, Option<Windowed>), Error> {
        let mut bytes = Vec::new();
        let windowed = self.object(repo, change, None, window, &mut |part| {
            bytes.extend_from_slice(&part);
            Ok(())
        })?;
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        // Built from `&str`s and escaped strings only.
        let object = String::from_utf8(bytes)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
        Ok((object, windowed))
    }
}

impl Files<'_> {
    /// The end object, without a line break: the counts over every file
    /// object made.
    pub(crate) fn end(&self) -> String {
        let t = &self.totals;
        format!(
            r#"{{"type":"end","files":{},"added":{},"removed":{}}}"#,
            t.files.load(Ordering::Relaxed),
            t.added.load(Ordering::Relaxed),
            t.removed.load(Ordering::Relaxed),
        )
    }

    /// Whether a file object made has a difference printed output would
    /// show: a file only `-w` or `-b` found unchanged has an object, but
    /// isn't one.
    pub(crate) fn differs(&self) -> bool {
        self.totals.differences.load(Ordering::Relaxed) > 0
    }

    fn count(&self, counts: Option<(usize, usize)>, whitespace_only: bool) {
        let t = &self.totals;
        let (added, removed) = counts.unwrap_or_default();
        t.files.fetch_add(1, Ordering::Relaxed);
        t.added.fetch_add(added, Ordering::Relaxed);
        t.removed.fetch_add(removed, Ordering::Relaxed);
        if !whitespace_only {
            t.differences.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A summary's file object without the syntax diff: counted as
    /// `--stat` counts, which builds no hunks, so the file's text is never
    /// kept and its language comes from its name alone.
    fn stat(
        &self,
        repo: &Repo,
        change: FileChange,
        emit: &mut dyn FnMut(Vec<u8>) -> Result<(), Error>,
    ) -> Result<bool, Error> {
        let whitespace = self.options.whitespace;
        let stat = match &self.stats {
            Some(stats) => stats.stat(repo, &change, whitespace),
            None => repo.stat(&change, whitespace),
        };
        let (stat, note) = match stat {
            Ok(stat) => (Some(stat), None),
            Err(e @ dffy_core::git::Error::Io { .. }) => (None, Some(e.to_string())),
            Err(e) => return Err(e.into()),
        };
        let sizes = stat.and_then(|s| s.binary);
        let too_large = sizes.is_some_and(|(old, new)| old.max(new) >= MAX_DIFF_BYTES);
        let fd = FileDiff {
            change,
            old: Text::default(),
            new: Text::default(),
            binary: sizes.is_some() && !too_large,
            too_large,
            note,
            hunks: Vec::new(),
        };
        let counts = stat
            .filter(|s| s.binary.is_none())
            .map(|s| (s.added, s.removed));
        // Without hunks to look at, no changed lines is what -w leaves.
        let whitespace_only = counts == Some((0, 0)) && document::whitespace_only(&fd, whitespace);
        self.count(counts, whitespace_only);
        let file = FileFields {
            language: language(self.grammars, &fd),
            counts,
            syntax: None,
            classification: classification(&fd, whitespace_only, None),
        };
        let mut s = String::new();
        file_fields(&fd, &file, &mut s);
        s.push_str(",\"hunks\":null}\n");
        emit(s.into_bytes())?;
        Ok(true)
    }
}

impl FileOutput for Files<'_> {
    const SEPARATOR: &'static [u8] = b"";

    fn file(
        &self,
        repo: &Repo,
        change: FileChange,
        sizes: Option<[u64; 2]>,
        emit: &mut dyn FnMut(Vec<u8>) -> Result<(), Error>,
    ) -> Result<bool, Error> {
        self.object(repo, change, sizes, None, emit).map(|_| true)
    }
}

impl Files<'_> {
    /// Diffs `change` and hands its file object to `emit` in parts, its
    /// hunks limited to `window` when there is one.
    fn object(
        &self,
        repo: &Repo,
        change: FileChange,
        sizes: Option<[u64; 2]>,
        window: Option<Window>,
        emit: &mut dyn FnMut(Vec<u8>) -> Result<(), Error>,
    ) -> Result<Option<Windowed>, Error> {
        if self.summary && !self.syntax_diff {
            self.stat(repo, change, emit)?;
            return Ok(None);
        }
        let mut fd = diff(repo, change, sizes, &self.options)?;
        let prepare = File::prepare(self.grammars, self.cache);
        let whitespace_only = document::whitespace_only(&fd, self.options.whitespace);
        let structural = self.syntax_diff.then(|| prepare.structural(&fd)).flatten();
        // The syntax diff keeps the line diff's lines, so they count alike.
        let (mut added, mut removed) = (0, 0);
        for line in fd.hunks.iter().flat_map(|h| &h.lines) {
            match line.kind {
                LineKind::Added => added += 1,
                LineKind::Removed => removed += 1,
                LineKind::Context => {}
            }
        }
        let counts = (!fd.binary && !fd.too_large).then_some((added, removed));
        self.count(counts, whitespace_only);
        let mut s = String::new();
        let file = FileFields {
            language: language(self.grammars, &fd),
            counts,
            syntax: self.syntax_diff.then_some(structural.as_ref()).flatten(),
            classification: classification(&fd, whitespace_only, structural.as_ref()),
        };
        file_fields(&fd, &file, &mut s);
        if self.summary {
            s.push_str(",\"hunks\":null}\n");
            emit(s.into_bytes())?;
            return Ok(None);
        }
        // A file paged through has them in its first part only.
        if let Some((comments, reverse)) = self.comments
            && window.is_none_or(|w| (w.hunk, w.line) == (0, 0))
        {
            annotations(&comments.on(&fd.change, &fd, reverse), reverse, &mut s);
        }
        // The syntax diff pairs its lines and sets their emphasis itself.
        let plain = if matches!(structural, Some(Ok(_))) {
            Vec::new()
        } else {
            document::pair(&fd.old, &fd.new, &mut fd.hunks, true)
        };
        let syntax = self.highlights.then(|| {
            let [old, new] = document::sources(&fd);
            let spans = |side: Source, lines: fn(&Hunk) -> Range<usize>| {
                let h = prepare.highlighter(side)?;
                Some(
                    fd.hunks
                        .iter()
                        .map(|hunk| h.spans(side.text, lines(hunk)))
                        .collect::<Vec<_>>(),
                )
            };
            let old = spans(old, |h| h.old_start..h.old_start + h.old_len);
            let new = spans(new, |h| h.new_start..h.new_start + h.new_len);
            let (mut old, mut new) = (old.into_iter().flatten(), new.into_iter().flatten());
            fd.hunks
                .iter()
                .map(|_| HunkSyntax {
                    old: old.next(),
                    new: new.next(),
                })
                .collect::<Vec<_>>()
        });
        let hunks = match &structural {
            Some(Ok(s)) => &s.hunks,
            _ => &fd.hunks,
        };
        let cut = write_hunks(&fd, hunks, &plain, syntax.as_deref(), window, s, emit)?;
        Ok(window.map(|_| Windowed {
            cut,
            digest: digest(&fd),
        }))
    }
}

/// Both sides' bytes hashed. The digest only has to match across calls of
/// one build, which `DefaultHasher::new` guarantees.
fn digest(fd: &FileDiff) -> u64 {
    use std::hash::{DefaultHasher, Hasher};
    let mut h = DefaultHasher::new();
    h.write_usize(fd.old.bytes().len());
    h.write(fd.old.bytes());
    h.write(fd.new.bytes());
    h.finish()
}

/// The rest of a file object: `hunks`, of `fd`, with each one's `plain`
/// emphasis when it has none of its own and `syntax` spans when
/// highlighted, appended to `s` and handed to `emit` in parts. With a
/// `window`, only its part; returns where that part stopped short of the
/// end.
fn write_hunks(
    fd: &FileDiff,
    hunks: &[Hunk],
    plain: &[Vec<diff::Emphasis>],
    syntax: Option<&[HunkSyntax]>,
    window: Option<Window>,
    mut s: String,
    emit: &mut dyn FnMut(Vec<u8>) -> Result<(), Error>,
) -> Result<Option<Cut>, Error> {
    s.push_str(r#","hunks":["#);
    let mut heading = Heading::default();
    let first = window.map_or(0, |w| w.hunk);
    let (mut lines_out, mut bytes_out, mut written) = (0, 0, 0);
    let mut cut = None;
    for (i, hunk) in hunks.iter().enumerate().skip(first) {
        let start = match window {
            Some(w) if i == first => w.line,
            _ => 0,
        };
        if window.is_some() && start >= hunk.lines.len() {
            continue;
        }
        let mut end = hunk.lines.len();
        if let Some(w) = window {
            end = w.end(fd, &hunk.lines, start, (lines_out, bytes_out));
            if end == start {
                cut = Some((i, start));
                break;
            }
            lines_out += end - start;
            bytes_out += text_bytes(fd, &hunk.lines[start..end]);
        }
        if written > 0 {
            s.push(',');
        }
        written += 1;
        let part;
        let numbered = if start == 0 && end == hunk.lines.len() {
            hunk
        } else {
            let lines = &hunk.lines[start..end];
            part = Hunk {
                old_start: lines[0].old,
                old_len: lines.iter().filter(|l| l.kind != LineKind::Added).count(),
                new_start: lines[0].new,
                new_len: lines.iter().filter(|l| l.kind != LineKind::Removed).count(),
                lines: Vec::new(),
            };
            &part
        };
        let h = document::hunk_header(numbered, heading.find(&fd.old, numbered.old_start));
        let _ = write!(
            s,
            r#"{{"old_start":{},"old_lines":{},"new_start":{},"new_lines":{},"heading":"#,
            h.old_start, h.old_len, h.new_start, h.new_len
        );
        if h.heading.is_empty() {
            s.push_str("null");
        } else {
            string(&mut s, &String::from_utf8_lossy(h.heading));
        }
        s.push_str(r#","lines":["#);
        let spans = syntax.and_then(|s| s.get(i));
        for (j, line) in hunk.lines.iter().enumerate().take(end).skip(start) {
            if j > start {
                s.push(',');
            }
            let emphasis = match plain.get(i) {
                Some(e) => &e[j][..],
                None => &line.emphasis[..],
            };
            let runs = spans.and_then(|spans| match line.kind {
                LineKind::Removed => spans.old.as_ref().map(|o| (o, line.old)),
                LineKind::Added | LineKind::Context => spans.new.as_ref().map(|n| (n, line.new)),
            });
            line_object(&fd.old, &fd.new, line, emphasis, runs, &mut s);
            if s.len() >= PART_BYTES {
                emit(std::mem::take(&mut s).into_bytes())?;
            }
        }
        s.push_str("]}");
        if end < hunk.lines.len() {
            cut = Some((i, end));
            break;
        }
    }
    s.push_str("]}\n");
    emit(s.into_bytes())?;
    Ok(cut.map(|(hunk, line)| {
        let left = hunks[hunk].lines.len() - line;
        let after: usize = hunks[hunk + 1..].iter().map(|h| h.lines.len()).sum();
        Cut {
            at: (hunk, line),
            remaining: left + after,
        }
    }))
}

/// The bytes of `lines`' text, terminators included.
fn text_bytes(fd: &FileDiff, lines: &[diff::Line]) -> usize {
    lines
        .iter()
        .map(|line| line_bytes(&fd.old, &fd.new, line).len())
        .sum()
}

impl Window {
    /// Where the part of a hunk of `lines` from `start` that this window
    /// holds ends, when `taken` (lines, bytes) are already in it; `start`
    /// when the hunk waits for the next window.
    fn end(
        &self,
        fd: &FileDiff,
        lines: &[diff::Line],
        start: usize,
        taken: (usize, usize),
    ) -> usize {
        let (lines_out, bytes_out) = taken;
        let whole = lines.len() - start;
        let bytes = text_bytes(fd, &lines[start..]);
        if lines_out + whole <= self.max_lines && bytes_out + bytes <= self.max_bytes {
            return lines.len();
        }
        let room = self.max_lines - lines_out;
        let fits_alone = whole <= self.max_lines && bytes <= self.max_bytes;
        if lines_out > 0 && (fits_alone || room == 0) {
            return start;
        }
        // A hunk larger than a whole window is cut by lines, filling this
        // one; a window's first line is always kept, so every window moves
        // on.
        let mut end = start;
        let mut held = bytes_out;
        while end < lines.len() && end - start < room {
            let next = text_bytes(fd, &lines[end..=end]);
            if held + next > self.max_bytes && (end > start || lines_out > 0) {
                break;
            }
            held += next;
            end += 1;
        }
        end
    }
}

/// `change`'s diff. A file that can't be read has none, and a note naming
/// the path that failed, so the files after it are still written.
fn diff(
    repo: &Repo,
    change: FileChange,
    sizes: Option<[u64; 2]>,
    options: &Options,
) -> Result<FileDiff, Error> {
    let diffed = match sizes {
        Some(sizes) => repo.file_diff_sized(change.clone(), sizes, options),
        None => repo.file_diff(change.clone(), options),
    };
    match diffed {
        Err(e @ dffy_core::git::Error::Io { .. }) => Ok(FileDiff {
            change,
            old: Text::default(),
            new: Text::default(),
            binary: false,
            too_large: false,
            note: Some(e.to_string()),
            hunks: Vec::new(),
        }),
        other => other.map_err(Error::from),
    }
}

/// What a file object says beyond its change and diff.
struct FileFields<'a> {
    language: Option<&'a str>,
    /// Added and removed lines; `None` for a file without a line diff.
    counts: Option<(usize, usize)>,
    /// The syntax diff, when it's on and the file has lines to diff.
    syntax: Option<&'a Result<Structural, Unavailable>>,
    classification: Option<&'static str>,
}

/// A file object's fields before its hunks, the object left open.
fn file_fields(fd: &FileDiff, file: &FileFields, s: &mut String) {
    let header = document::file_header(&fd.change, None);
    let status = match fd.change.status {
        Status::Added => "added",
        Status::Deleted => "deleted",
        Status::Modified => "modified",
        Status::TypeChange => "type_changed",
        Status::Untracked => "untracked",
        Status::Unmerged => "unmerged",
        Status::Renamed { .. } => "renamed",
    };
    s.push_str(r#"{"type":"file","path":"#);
    string(s, &String::from_utf8_lossy(header.new_path));
    s.push_str(r#","old_path":"#);
    string(s, &String::from_utf8_lossy(header.old_path));
    let _ = write!(s, r#","status":"{status}","similarity":"#);
    optional(s, header.similarity);
    let mode = |s: &mut String, mode: Option<&str>| match mode {
        Some(m) => {
            let _ = write!(s, r#""{m}""#);
        }
        None => s.push_str("null"),
    };
    s.push_str(r#","old_mode":"#);
    mode(s, header.old_mode);
    s.push_str(r#","new_mode":"#);
    mode(s, header.new_mode);
    let _ = write!(
        s,
        r#","binary":{},"too_large":{},"language":"#,
        fd.binary, fd.too_large
    );
    match file.language {
        Some(name) => string(s, name),
        None => s.push_str("null"),
    }
    s.push_str(r#","added":"#);
    optional(s, file.counts.map(|c| c.0));
    s.push_str(r#","removed":"#);
    optional(s, file.counts.map(|c| c.1));
    let syntax = file.syntax.map(|result| match result {
        Ok(s) if s.regions.fell_back > 0 => "partial",
        Ok(_) => "complete",
        Err(Unavailable::Language) => "unknown_language",
        Err(Unavailable::TooLarge) => "too_large",
        Err(Unavailable::Parse) => "parse_timeout",
    });
    s.push_str(r#","syntax":"#);
    quoted_or_null(s, syntax);
    s.push_str(r#","classification":"#);
    quoted_or_null(s, file.classification);
    s.push_str(r#","note":"#);
    match &fd.note {
        Some(note) => string(s, note),
        None => s.push_str("null"),
    }
}

/// `,"annotations":[…]`: each of `threads` on lines, its source, where
/// it's placed or lost, with its comments as the comments file holds them. `reverse`
/// (`-R`) swaps the side a lost one was written on, as placing swaps a
/// placed one's.
fn annotations(threads: &[Hung], reverse: bool, s: &mut String) {
    s.push_str(r#","annotations":["#);
    let mut first = true;
    for hung in threads {
        let Some(root) = hung.thread.first() else {
            continue;
        };
        let mut place = String::new();
        if !placement(&hung.at, root, reverse, &mut place) {
            continue;
        }
        if !first {
            s.push(',');
        }
        first = false;
        s.push_str(r#"{"id":"#);
        string(s, &root.id);
        s.push_str(r#","source":"#);
        string(s, review::model::SOURCE);
        s.push(',');
        s.push_str(&place);
        s.push_str(r#","thread":["#);
        for (i, comment) in hung.thread.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            crate::plugin::json::write(s, &Value::Object(comment.to_fields()));
        }
        s.push_str("]}");
    }
    s.push(']');
}

/// The members `"side":…,"start":…,"end":…,"lost":…` of a line thread
/// starting with `root`, `at` its place: the side it's on and its lines
/// there, 1-based, or the side it was written on and null lines when it's
/// lost; `reverse` (`-R`) swaps that side as placing swaps a placed one's.
/// Returns false, writing nothing, for a thread that isn't on lines.
pub(crate) fn placement(at: &At, root: &review::Comment, reverse: bool, s: &mut String) -> bool {
    let (side, lines) = match (at, root.range()) {
        (At::Line { side, lines }, _) => (*side, Some(lines)),
        (At::Lost, Some((side, _))) => {
            let old = (side == review::Side::Old) != reverse;
            (if old { Side::Old } else { Side::New }, None)
        }
        _ => return false,
    };
    let _ = write!(s, r#""side":"{}","start":"#, self::side(side));
    optional(s, lines.map(|l| l.start + 1));
    s.push_str(r#","end":"#);
    optional(s, lines.map(|l| l.end));
    let _ = write!(s, r#","lost":{}"#, lines.is_none());
    true
}

pub(crate) fn side(side: Side) -> &'static str {
    match side {
        Side::Old => "old",
        Side::New => "new",
    }
}

fn optional(s: &mut String, n: Option<impl std::fmt::Display>) {
    match n {
        Some(n) => {
            let _ = write!(s, "{n}");
        }
        None => s.push_str("null"),
    }
}

/// A word known to need no escaping, quoted; `null` without one.
fn quoted_or_null(s: &mut String, word: Option<&str>) {
    match word {
        Some(w) => {
            let _ = write!(s, r#""{w}""#);
        }
        None => s.push_str("null"),
    }
}

/// The language the file is highlighted and syntax-diffed in, detected as
/// for those: by the new side's path and first bytes, or the old side's
/// for a deletion.
fn language<'a>(grammars: &'a Registry, fd: &FileDiff) -> Option<&'a str> {
    let (path, text) = if fd.new.is_empty() && !fd.old.is_empty() {
        (fd.change.old_path().as_ref(), &fd.old)
    } else {
        (fd.change.path.as_ref(), &fd.new)
    };
    let head = &text.bytes()[..text.bytes().len().min(256)];
    grammars
        .detect(path, head)
        .map(dffy_core::highlight::Language::name)
}

/// `formatting_only` when every change is whitespace or reformatting,
/// `moved_only` when every changed line moved; `None` otherwise. Only an
/// in-place edit of the same kind of file qualifies: a rename, a mode or
/// type change is a change of its own.
fn classification(
    fd: &FileDiff,
    whitespace_only: bool,
    structural: Option<&Result<Structural, Unavailable>>,
) -> Option<&'static str> {
    if whitespace_only {
        return Some("formatting_only");
    }
    let Some(Ok(s)) = structural else {
        return None;
    };
    let same_mode = fd.change.old.map(|s| s.mode()) == fd.change.new.map(|s| s.mode());
    if fd.change.status != Status::Modified || !same_mode || fd.note.is_some() {
        return None;
    }
    let mut changed = s
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.kind != LineKind::Context)
        .map(|l| l.unchanged);
    let first = changed.next()?;
    if !changed.all(|u| u == first) {
        return None;
    }
    match first {
        Unchanged::Layout => Some("formatting_only"),
        Unchanged::Moved => Some("moved_only"),
        Unchanged::No => None,
    }
}

/// A line object: `line` of a hunk of `old` and `new`, with its emphasis
/// and, when its side is highlighted, the side's spans and its line there.
fn line_object(
    old: &Text,
    new: &Text,
    line: &diff::Line,
    emphasis: &[Range<usize>],
    runs: Option<(&Spans, usize)>,
    s: &mut String,
) {
    let kind = match line.kind {
        LineKind::Context => "context",
        LineKind::Removed => "removed",
        LineKind::Added => "added",
    };
    let _ = write!(s, r#"{{"kind":"{kind}""#);
    let mark = match line.unchanged {
        Unchanged::Moved => Some("moved"),
        Unchanged::Layout => Some("reformatted"),
        Unchanged::No if line.paired => Some("modified"),
        Unchanged::No => None,
    };
    if let Some(mark) = mark.filter(|_| line.kind != LineKind::Context) {
        let _ = write!(s, r#","mark":"{mark}""#);
    }
    s.push_str(r#","old":"#);
    optional(s, (line.kind != LineKind::Added).then_some(line.old + 1));
    s.push_str(r#","new":"#);
    optional(s, (line.kind != LineKind::Removed).then_some(line.new + 1));
    let raw = line_bytes(old, new, line);
    let content = raw.strip_suffix(b"\n");
    let no_newline = content.is_none();
    let content = content.unwrap_or(raw);
    s.push_str(r#","text":"#);
    let text = std::str::from_utf8(content);
    if let Ok(text) = text {
        string(s, text);
    } else {
        string(s, &String::from_utf8_lossy(content));
        s.push_str(r#","lossy":true"#);
    }
    if no_newline {
        s.push_str(r#","no_newline":true"#);
    }
    if line.block_start {
        s.push_str(r#","block_start":true"#);
    }
    // Offsets index the text's UTF-8 bytes, which a lossy line's aren't.
    if text.is_ok() {
        ranges(
            s,
            "emphasis",
            emphasis.iter().map(|r| (r.start, r.end, None)),
        );
        if let Some((spans, i)) = runs {
            let runs = spans
                .line(i)
                .iter()
                .map(|run| (run.start as usize, run.end as usize, Some(run.syntax.key())));
            ranges(s, "highlights", runs);
        }
    }
    s.push('}');
}

/// `"name":[[start,end],…]`, or `[start,end,"role"]` with a role; nothing
/// when there are no ranges.
fn ranges(
    s: &mut String,
    name: &str,
    ranges: impl Iterator<Item = (usize, usize, Option<&'static str>)>,
) {
    let mut ranges = ranges.peekable();
    if ranges.peek().is_none() {
        return;
    }
    let _ = write!(s, r#","{name}":["#);
    for (i, (start, end, role)) in ranges.enumerate() {
        let comma = if i > 0 { "," } else { "" };
        let _ = write!(s, "{comma}[{start},{end}");
        if let Some(role) = role {
            let _ = write!(s, r#","{role}""#);
        }
        s.push(']');
    }
    s.push(']');
}

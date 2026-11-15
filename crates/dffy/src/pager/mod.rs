//! Pager mode: re-renders a unified diff read from stdin, as `core.pager`
//! or at the end of a pipe. Output streams hunk by hunk; memory is bounded
//! by the longest hunk part kept for pairing, not by the input.

pub(crate) mod color_only;
pub(crate) mod parse;
pub(crate) mod stdin;

use std::io::{self, BufRead, BufReader, Read};
use std::ops::Range;

use dffy_core::diff::{Emphasis, emphasize_lines};

use crate::render::{
    Canvas, DiffLine, FileHeader, FileStatus, HunkHeader, Kind, Layout, display_text,
};
use crate::theme::Role;
use parse::{Event, FileInfo, Parser, TextKind};

/// Calls `line` with each line of `input`, without its newline and with
/// escape sequences removed, and `blocked` before a read that may wait.
pub(crate) fn for_each_line<E: From<io::Error>>(
    input: impl Read,
    mut blocked: impl FnMut() -> Result<(), E>,
    mut line: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<(), E> {
    let mut input = BufReader::with_capacity(64 << 10, input);
    let mut raw = Vec::with_capacity(256);
    let mut clean = Vec::with_capacity(256);
    loop {
        if !input.buffer().contains(&b'\n') {
            blocked()?;
        }
        raw.clear();
        if input.read_until(b'\n', &mut raw)? == 0 {
            return Ok(());
        }
        let text = raw.strip_suffix(b"\n").unwrap_or(&raw);
        let text = if text.contains(&0x1b) {
            parse::strip_ansi(text, &mut clean);
            &clean[..]
        } else {
            text
        };
        line(text)?;
    }
}

/// Hunks longer than this are rendered in parts, so a huge new file streams
/// instead of being held whole.
const PART_LINES: usize = 1024;

/// A change block (removed lines then added lines) is held whole for
/// pairing up to this many lines; past it, it's flushed unpaired.
const MAX_BLOCK_LINES: usize = 16 * 1024;

/// What the renderer hands its output to: `Write(canvas)` for rendered
/// rows, `Flush` when the input is about to block.
pub(crate) enum Sink<'c> {
    Write(&'c Canvas),
    Flush,
}

/// Reads a diff from `input` and passes it rendered to `sink`.
pub(crate) fn run(
    input: impl Read,
    layout: Layout,
    sink: impl FnMut(Sink) -> io::Result<()>,
) -> io::Result<()> {
    let renderer = std::cell::RefCell::new(Renderer {
        layout,
        file_layout: layout,
        next_old: 1,
        sink,
        canvas: Canvas::default(),
        hunk: HunkBuf::default(),
        wrote_any: false,
        prev_blank: false,
    });
    let mut parser = Parser::default();
    for_each_line(
        input,
        // Reading a line that isn't fully buffered may block; show what's
        // rendered so far first.
        || (renderer.borrow_mut().sink)(Sink::Flush),
        |line| parser.feed(line, &mut |event| renderer.borrow_mut().event(event)),
    )?;
    parser.finish(&mut |event| renderer.borrow_mut().event(event))
}

/// The color of a combined diff's line, by its prefix columns (`parents`
/// of them, 0 before the first `@@@`).
pub(crate) fn combined_role(parents: usize, line: &[u8]) -> Role {
    if line.starts_with(b"@@@") {
        Role::HunkHeader
    } else if parents == 0 {
        Role::Dim
    } else {
        let prefix = &line[..parents.min(line.len())];
        if prefix.contains(&b'-') {
            Role::Removed
        } else if prefix.contains(&b'+') {
            Role::Added
        } else {
            Role::Context
        }
    }
}

/// Intra-line emphasis for `len` hunk lines, `line(i)` giving each one's
/// kind and text without its prefix, and whether each line was paired:
/// removed lines pair with the added lines right after them, within each
/// change block.
pub(crate) fn emphasis<'a>(
    len: usize,
    line: impl Fn(usize) -> (Kind, &'a [u8]),
) -> Vec<(Emphasis, bool)> {
    let mut out = vec![(Vec::new(), false); len];
    let mut i = 0;
    while i < len {
        if line(i).0 == Kind::Context {
            i += 1;
            continue;
        }
        let mut removed = Vec::new();
        let mut added = Vec::new();
        while i < len && line(i).0 != Kind::Context {
            match line(i).0 {
                Kind::Removed if added.is_empty() => removed.push(i),
                Kind::Removed => break,
                Kind::Added => added.push(i),
                Kind::Context | Kind::NoNewline => {}
            }
            i += 1;
        }
        if removed.is_empty() || added.is_empty() {
            continue;
        }
        let old: Vec<&[u8]> = removed.iter().map(|&k| line(k).1).collect();
        let new: Vec<&[u8]> = added.iter().map(|&k| line(k).1).collect();
        emphasize_lines(&old, &new, |a, b, old_emph, new_emph| {
            out[removed[a]] = (old_emph, true);
            out[added[b]] = (new_emph, true);
        });
    }
    out
}

struct Renderer<F> {
    layout: Layout,
    /// The layout for the current file's hunks.
    file_layout: Layout,
    /// The first old line of the current file no hunk has shown, 1-based.
    next_old: usize,
    sink: F,
    canvas: Canvas,
    hunk: HunkBuf,
    wrote_any: bool,
    prev_blank: bool,
}

impl<F: FnMut(Sink) -> io::Result<()>> Renderer<F> {
    fn event(&mut self, event: Event) -> io::Result<()> {
        match event {
            Event::Text(kind, text) => {
                let role = match kind {
                    TextKind::Plain | TextKind::CommitMessage => Role::Plain,
                    TextKind::CommitHeader => Role::CommitHeader,
                    TextKind::CommitMeta | TextKind::Meta => Role::Dim,
                };
                self.layout
                    .text_line(role, display_text(text), &mut self.canvas);
                self.prev_blank = text.is_empty();
            }
            Event::File(info) => self.file(info),
            Event::Hunk(h) => {
                // A hunk without old lines comes after its start line.
                let first = h.old_start + usize::from(h.old_len == 0);
                let hidden = first.saturating_sub(self.next_old);
                self.next_old = first + h.old_len;
                self.hunk.start(h, hidden);
                return Ok(());
            }
            Event::Line(kind, text) => {
                if self.hunk.active {
                    self.hunk.push(kind, text);
                    let Some(end) = self.hunk.part_end() else {
                        return Ok(());
                    };
                    self.render_part(end);
                } else {
                    self.layout.text_line(Role::Dim, text, &mut self.canvas);
                }
            }
            Event::HunkEnd => {
                self.render_part(self.hunk.lines.len());
                self.hunk.active = false;
            }
            Event::Stray(text) => {
                self.layout
                    .text_line(Role::Dim, display_text(text), &mut self.canvas);
            }
            Event::Combined { parents, line } => {
                self.layout.text_line(
                    combined_role(parents, line),
                    display_text(line),
                    &mut self.canvas,
                );
            }
        }
        self.write()
    }

    fn write(&mut self) -> io::Result<()> {
        if !self.canvas.is_empty() {
            (self.sink)(Sink::Write(&self.canvas))?;
            self.canvas.clear();
            self.wrote_any = true;
        }
        Ok(())
    }

    fn file(&mut self, info: &FileInfo) {
        if self.wrote_any && !self.prev_blank {
            self.canvas.end_line();
        }
        self.prev_blank = false;
        let renamed = matches!(info.status, FileStatus::Renamed | FileStatus::Copied);
        self.file_layout = self.layout.for_file(info.status, 0);
        self.next_old = 1;
        self.layout.file_header(
            &FileHeader {
                status: info.status,
                old_path: &info.old_path,
                new_path: &info.new_path,
                old_mode: info.old_mode.as_deref(),
                new_mode: info.new_mode.as_deref(),
                similarity: info.similarity.filter(|_| renamed),
                note: info.binary.then_some("binary files differ"),
                staging: None,
            },
            &mut self.canvas,
        );
    }

    /// Renders the buffered hunk lines before `end` and drops them.
    fn render_part(&mut self, end: usize) {
        let h = &mut self.hunk;
        if !h.active {
            return;
        }
        let header = HunkHeader {
            old_start: h.old_start,
            old_len: h.old_len,
            new_start: h.new_start,
            new_len: h.new_len,
            heading: &h.heading,
            hidden: h.hidden,
        };
        let emphasis = emphasis(end, |i| {
            let l = &h.lines[i];
            (l.kind, &h.text[l.range.clone()])
        });
        let lines: Vec<DiffLine> = h.lines[..end]
            .iter()
            .zip(&emphasis)
            .map(|(l, (emphasis, paired))| DiffLine {
                kind: l.kind,
                old: l.old,
                new: l.new,
                text: if l.kind == Kind::NoNewline {
                    &h.text[l.range.clone()]
                } else {
                    display_text(&h.text[l.range.clone()])
                },
                emphasis,
                // Not highlighted: compiling one language's query alone
                // costs up to 35ms, over the 20ms budget for first output,
                // and hunk fragments parse poorly without their file.
                syntax: None,
                paint: &[],
                unchanged: dffy_core::diff::Unchanged::No,
                modified: *paired,
                block_start: false,
            })
            .collect();
        self.file_layout
            .hunk(&header, !h.header_shown, &lines, &mut self.canvas);
        h.header_shown = true;
        h.drop_front(end);
    }
}

#[derive(Debug, Clone)]
struct BufLine {
    kind: Kind,
    range: Range<usize>,
    old: Option<usize>,
    new: Option<usize>,
}

/// The current hunk's lines not yet rendered.
#[derive(Debug, Default)]
struct HunkBuf {
    active: bool,
    old_start: usize,
    old_len: usize,
    new_start: usize,
    new_len: usize,
    heading: Vec<u8>,
    hidden: usize,
    header_shown: bool,
    next_old: usize,
    next_new: usize,
    text: Vec<u8>,
    lines: Vec<BufLine>,
    /// Index of the first line after the last context line: the trailing
    /// change block, which may still be waiting for lines to pair with.
    block_start: usize,
    block_has_removed: bool,
}

impl HunkBuf {
    fn start(&mut self, h: parse::Hunk, hidden: usize) {
        self.active = true;
        self.hidden = hidden;
        self.old_start = h.old_start;
        self.old_len = h.old_len;
        self.new_start = h.new_start;
        self.new_len = h.new_len;
        self.heading.clear();
        self.heading.extend_from_slice(h.heading);
        self.header_shown = false;
        self.next_old = h.old_start;
        self.next_new = h.new_start;
        self.text.clear();
        self.lines.clear();
        self.block_start = 0;
        self.block_has_removed = false;
    }

    fn push(&mut self, kind: Kind, text: &[u8]) {
        let start = self.text.len();
        self.text.extend_from_slice(text);
        let (old, new) = match kind {
            Kind::Context => (Some(self.next_old), Some(self.next_new)),
            Kind::Removed => (Some(self.next_old), None),
            Kind::Added => (None, Some(self.next_new)),
            Kind::NoNewline => (None, None),
        };
        self.next_old += usize::from(old.is_some());
        self.next_new += usize::from(new.is_some());
        self.lines.push(BufLine {
            kind,
            range: start..self.text.len(),
            old,
            new,
        });
        match kind {
            Kind::Context => {
                self.block_start = self.lines.len();
                self.block_has_removed = false;
            }
            Kind::Removed => self.block_has_removed = true,
            Kind::Added | Kind::NoNewline => {}
        }
    }

    /// Where to cut a part when the buffer is long: everything, unless the
    /// trailing change block still waits for added lines to pair with.
    fn part_end(&self) -> Option<usize> {
        if self.lines.len() < PART_LINES {
            return None;
        }
        let block_len = self.lines.len() - self.block_start;
        if !self.block_has_removed || block_len >= MAX_BLOCK_LINES {
            Some(self.lines.len())
        } else if self.block_start > 0 {
            Some(self.block_start)
        } else {
            None
        }
    }

    /// Drops the first `end` lines; `end` is either everything or the start
    /// of the trailing block.
    fn drop_front(&mut self, end: usize) {
        if end >= self.lines.len() {
            self.lines.clear();
            self.text.clear();
            self.block_start = 0;
            self.block_has_removed = false;
            return;
        }
        debug_assert!(end <= self.block_start);
        self.block_start -= end;
        let offset = self.lines[end].range.start;
        self.lines.drain(..end);
        self.text.drain(..offset);
        for l in &mut self.lines {
            l.range = l.range.start - offset..l.range.end - offset;
        }
    }
}

#[cfg(test)]
mod tests;

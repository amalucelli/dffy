//! Pager mode: re-renders a unified diff read from stdin, as `core.pager`
//! or at the end of a pipe. Output streams hunk by hunk; memory is bounded
//! by the longest hunk part kept for pairing, not by the input.

pub(crate) mod color_only;
pub(crate) mod stdin;

use std::io::{self, BufRead, BufReader, Read};
use std::ops::Range;

use dffy_core::diff::{Unchanged, pair_lines};
use dffy_core::patch::{self, CombinedKind, Event, FileInfo, Parser, TextKind, strip_ansi};
use dffy_core::view::{FileHeader, FileStatus, HunkHeader, Kind, Line, LineState, display_text};

use crate::render::{Canvas, Layout};
use crate::theme::Role;

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
            strip_ansi(text, &mut clean);
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

/// The color of a line of text outside the diff: a commit's header, its
/// metadata, its message, or anything else.
pub(crate) fn text_role(kind: TextKind) -> Role {
    match kind {
        TextKind::Plain | TextKind::CommitMessage => Role::Plain,
        TextKind::CommitHeader => Role::CommitHeader,
        TextKind::CommitMeta | TextKind::Meta => Role::Dim,
    }
}

/// The color of a combined diff's line.
pub(crate) fn combined_role(kind: CombinedKind) -> Role {
    match kind {
        CombinedKind::Header => Role::Dim,
        CombinedKind::HunkHeader => Role::HunkHeader,
        CombinedKind::Removed => Role::Removed,
        CombinedKind::Added => Role::Added,
        CombinedKind::Context => Role::Context,
    }
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
                self.layout
                    .text_line(text_role(kind), display_text(text), &mut self.canvas);
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
                    combined_role(patch::combined_kind(parents, line)),
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
        let emphasis = pair_lines(end, |i| {
            let l = &h.lines[i];
            (l.kind, &h.text[l.range.clone()])
        });
        let lines: Vec<Line> = h.lines[..end]
            .iter()
            .zip(&emphasis)
            .map(|(l, (emphasis, paired))| Line {
                state: LineState::new(l.kind, *paired, Unchanged::No),
                old: l.old,
                new: l.new,
                text: if l.kind == Kind::NoNewline {
                    &h.text[l.range.clone()]
                } else {
                    display_text(&h.text[l.range.clone()])
                },
                emphasis,
                // Not highlighted: compiling one language's query alone
                // costs up to about 140ms (Swift's), over the 20ms budget
                // for first output, and hunk fragments parse poorly
                // without their file.
                syntax: None,
                paint: &[],
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
    fn start(&mut self, h: patch::Hunk, hidden: usize) {
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

//! Sequence diagrams: a column per participant with its lifeline, then a
//! group of rows per statement, so each message, note and frame border is
//! a block of its own with its source line.
//!
//! Activations are left out, and so are `box` groups (their participants
//! show as usual); `rect` highlights are dropped. Participant boxes show
//! once, at the top.

use std::ops::Range;

use crate::canvas::{self, BORDER, Canvas, EDGE, KEYWORD, NUMBER, PLAIN, Stroke, TITLE};
use crate::{Block, MAX_CELLS, clean, wrap};

/// Past these sizes the source shows instead; the parser stops as soon as
/// one is passed.
const MAX_PARTICIPANTS: usize = 60;
const MAX_STATEMENTS: usize = 2000;

/// Message and note wrap widths to try, widest first, until the drawing
/// fits the width it's given.
const ATTEMPTS: [usize; 4] = [usize::MAX, 32, 20, 12];

/// Rounds of widening columns so frames clear the lifelines outside them.
const WIDENINGS: usize = 32;

struct Participant {
    id: String,
    label: String,
    actor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tip {
    None,
    Arrow,
    Open,
    Cross,
}

enum Place {
    Left(usize),
    Right(usize),
    Over(usize, usize),
}

enum Item {
    Message {
        from: usize,
        to: usize,
        stroke: Stroke,
        tip: Tip,
        /// A head at the sender too.
        both: bool,
        text: String,
        number: Option<u64>,
    },
    Note {
        place: Place,
        text: String,
    },
    /// `loop`, `alt`, `opt`, `par`, `critical` or `break`.
    Open {
        keyword: String,
        label: String,
    },
    /// `else`, `and` or `option`.
    Divider {
        keyword: String,
        label: String,
    },
    Close,
}

struct Diagram {
    participants: Vec<Participant>,
    /// The `sequenceDiagram` line and the declarations right after it.
    header: Range<usize>,
    items: Vec<(usize, Item)>,
}

/// Arrows by their text, longest first where one starts another, with
/// their stroke, tip and whether both ends have it.
const ARROWS: [(&str, Stroke, Tip, bool); 10] = [
    ("<<-->>", Stroke::Dotted, Tip::Arrow, true),
    ("<<->>", Stroke::Light, Tip::Arrow, true),
    ("-->>", Stroke::Dotted, Tip::Arrow, false),
    ("-->", Stroke::Dotted, Tip::None, false),
    ("--x", Stroke::Dotted, Tip::Cross, false),
    ("--)", Stroke::Dotted, Tip::Open, false),
    ("->>", Stroke::Light, Tip::Arrow, false),
    ("->", Stroke::Light, Tip::None, false),
    ("-x", Stroke::Light, Tip::Cross, false),
    ("-)", Stroke::Light, Tip::Open, false),
];

/// The sequence diagram `statements` describe (the first is its
/// `sequenceDiagram` line), a block per statement.
pub(crate) fn render(statements: &[(usize, &str)], width: usize) -> Option<Vec<Block>> {
    let diagram = parse(statements)?;
    if diagram.participants.is_empty() {
        return None;
    }
    let mut chosen: Option<(Chart<'_>, usize, usize)> = None;
    for wrap_at in ATTEMPTS {
        let chart = Chart::new(&diagram, wrap_at);
        let (w, h) = chart.size();
        if chosen
            .as_ref()
            .is_none_or(|(_, narrowest, _)| w < *narrowest)
        {
            chosen = Some((chart, w, h));
        }
        if w <= width {
            break;
        }
    }
    let (chart, w, h) = chosen?;
    (w.saturating_mul(h) <= MAX_CELLS).then(|| chart.draw())
}

fn parse(statements: &[(usize, &str)]) -> Option<Diagram> {
    let (&(first, header), rest) = statements.split_first()?;
    if header != "sequenceDiagram" {
        return None;
    }
    let mut parser = Parser {
        diagram: Diagram {
            participants: Vec::new(),
            header: first..first + 1,
            items: Vec::new(),
        },
        frames: Vec::new(),
        numbering: None,
    };
    for &(line, statement) in rest {
        parser.statement(line, statement.trim_end_matches(';').trim())?;
    }
    parser.frames.is_empty().then_some(parser.diagram)
}

struct Parser {
    diagram: Diagram,
    /// The blocks open, innermost last: `true` for a drawn frame, `false`
    /// for a `box` or `rect`.
    frames: Vec<bool>,
    /// The next message number and the step, while `autonumber` is on.
    numbering: Option<(u64, u64)>,
}

impl Parser {
    fn statement(&mut self, line: usize, statement: &str) -> Option<()> {
        let (word, rest) = statement
            .split_once(char::is_whitespace)
            .map_or((statement, ""), |(w, r)| (w, r.trim()));
        if matches!(word, "participant" | "actor" | "create") && self.diagram.items.is_empty() {
            self.diagram.header.end = line + 1;
        }
        let item = match word {
            "participant" | "actor" => return self.declare(rest, word == "actor"),
            "create" => {
                let (kind, rest) = rest.split_once(char::is_whitespace)?;
                return matches!(kind, "participant" | "actor")
                    .then(|| self.declare(rest.trim(), kind == "actor"))?;
            }
            "autonumber" => {
                let mut numbers = rest.split_whitespace().map(str::parse::<u64>);
                self.numbering = (rest != "off").then(|| {
                    let start = numbers.next().and_then(Result::ok).unwrap_or(1);
                    (start, numbers.next().and_then(Result::ok).unwrap_or(1))
                });
                return Some(());
            }
            "activate" | "deactivate" | "destroy" | "link" | "links" | "properties" | "details" => {
                return Some(());
            }
            _ if ["title", "accTitle", "accDescr"]
                .iter()
                .any(|k| word.starts_with(k)) =>
            {
                return Some(());
            }
            "box" | "rect" => {
                self.frames.push(false);
                return Some(());
            }
            "loop" | "alt" | "opt" | "par" | "critical" | "break" => {
                self.frames.push(true);
                Item::Open {
                    keyword: word.to_owned(),
                    label: clean(rest).replace('\n', " "),
                }
            }
            "else" | "and" | "option" => {
                if self.frames.last() != Some(&true) {
                    return None;
                }
                Item::Divider {
                    keyword: word.to_owned(),
                    label: clean(rest).replace('\n', " "),
                }
            }
            "end" => {
                if !self.frames.pop()? {
                    return Some(());
                }
                Item::Close
            }
            _ if word.eq_ignore_ascii_case("note") => self.note(rest)?,
            _ => self.message(statement)?,
        };
        if self.diagram.items.len() >= MAX_STATEMENTS {
            return None;
        }
        self.diagram.items.push((line, item));
        Some(())
    }

    /// `A`, `A as Alice`.
    fn declare(&mut self, rest: &str, actor: bool) -> Option<()> {
        let rest = rest.split("@{").next().unwrap_or(rest).trim();
        let (id, label) = rest
            .split_once(" as ")
            .map_or((rest, rest), |(id, label)| (id.trim(), label.trim()));
        if id.is_empty() {
            return None;
        }
        let i = self.participant(id)?;
        let participant = &mut self.diagram.participants[i];
        participant.label = clean(label).replace('\n', " ");
        participant.actor = actor;
        Some(())
    }

    /// The participant `id`, added when it's new; `None` past
    /// [`MAX_PARTICIPANTS`].
    fn participant(&mut self, id: &str) -> Option<usize> {
        let id = clean(id);
        let participants = &mut self.diagram.participants;
        if let Some(i) = participants.iter().position(|p| p.id == id) {
            return Some(i);
        }
        if participants.len() >= MAX_PARTICIPANTS {
            return None;
        }
        participants.push(Participant {
            label: id.replace('\n', " "),
            id,
            actor: false,
        });
        Some(participants.len() - 1)
    }

    /// `right of A: text`, `left of A: text`, `over A,B: text`.
    fn note(&mut self, rest: &str) -> Option<Item> {
        let (place, text) = rest.split_once(':')?;
        let (how, who) = place.trim().split_once(char::is_whitespace)?;
        let who = who.trim();
        let place = match how.to_ascii_lowercase().as_str() {
            "right" | "left" => {
                let who = who.strip_prefix("of")?.trim();
                let i = self.participant(who)?;
                if how.eq_ignore_ascii_case("right") {
                    Place::Right(i)
                } else {
                    Place::Left(i)
                }
            }
            "over" => {
                let (a, b) = who.split_once(',').unwrap_or((who, who));
                let (a, b) = (self.participant(a.trim())?, self.participant(b.trim())?);
                Place::Over(a.min(b), a.max(b))
            }
            _ => return None,
        };
        Some(Item::Note {
            place,
            text: clean(text),
        })
    }

    /// `A->>B: text`, `A-->>+B: text`.
    fn message(&mut self, statement: &str) -> Option<Item> {
        let (at, &(arrow, stroke, tip, both)) = statement.char_indices().find_map(|(i, _)| {
            ARROWS
                .iter()
                .find(|a| statement[i..].starts_with(a.0))
                .map(|a| (i, a))
        })?;
        let from = statement[..at].trim();
        let rest = &statement[at + arrow.len()..];
        let (to, text) = rest.split_once(':').unwrap_or((rest, ""));
        let to = to.trim().trim_start_matches(['+', '-']).trim();
        if from.is_empty() || to.is_empty() {
            return None;
        }
        let (from, to) = (self.participant(from)?, self.participant(to)?);
        let number = self.numbering.as_mut().map(|(next, step)| {
            let n = *next;
            *next = next.saturating_add(*step);
            n
        });
        Some(Item::Message {
            from,
            to,
            stroke,
            tip,
            both,
            text: clean(text),
            number,
        })
    }
}

fn int(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn cell(n: i64) -> usize {
    usize::try_from(n).unwrap_or(0)
}

/// The columns an item or frame draws on, first and last, and the first
/// and last participants it involves.
type Extent = (i64, i64, usize, usize);

/// A frame's columns, from its `Open` to its `Close`.
struct FrameBox {
    open: usize,
    close: usize,
    lo: i64,
    hi: i64,
    /// The participants it encloses.
    first: usize,
    last: usize,
}

/// A diagram laid out: columns, and each item's text lines and rows.
struct Chart<'d> {
    diagram: &'d Diagram,
    /// Each item's text, wrapped.
    texts: Vec<Vec<String>>,
    /// Each participant's lifeline column.
    xs: Vec<i64>,
    frames: Vec<FrameBox>,
    /// Each item's first row and row count.
    rows: Vec<(usize, usize)>,
}

impl<'d> Chart<'d> {
    fn new(diagram: &'d Diagram, wrap_at: usize) -> Self {
        let texts = diagram
            .items
            .iter()
            .map(|(_, item)| match item {
                Item::Message { text, .. } | Item::Note { text, .. } => wrap(text, wrap_at),
                _ => Vec::new(),
            })
            .collect();
        let mut chart = Self {
            diagram,
            texts,
            xs: Vec::new(),
            frames: Vec::new(),
            rows: Vec::new(),
        };
        chart.columns();
        chart.stack();
        chart
    }

    fn widest(&self, k: usize) -> i64 {
        let number = match &self.diagram.items[k].1 {
            Item::Message {
                number: Some(n), ..
            } => int(n.to_string().len()) + 1,
            _ => 0,
        };
        number
            + int(self.texts[k]
                .iter()
                .map(|l| canvas::width(l))
                .max()
                .unwrap_or(0))
    }

    fn box_width(&self, p: usize) -> i64 {
        int(canvas::width(&self.diagram.participants[p].label)) + 4
    }

    /// The least distance each pair of lifelines needs, as `(a, b, d)`
    /// for `x[b] - x[a] >= d`.
    fn needs(&self) -> Vec<(usize, usize, i64)> {
        let count = self.diagram.participants.len();
        let mut needs: Vec<(usize, usize, i64)> = (1..count)
            .map(|p| {
                let (a, b) = (self.box_width(p - 1), self.box_width(p));
                (p - 1, p, a - a / 2 + b / 2 + 1)
            })
            .collect();
        for (k, (_, item)) in self.diagram.items.iter().enumerate() {
            let w = self.widest(k);
            match *item {
                Item::Message { from, to, .. } if from == to => {
                    if from + 1 < count {
                        needs.push((from, from + 1, w + 7));
                    }
                }
                Item::Message { from, to, .. } => needs.push((from.min(to), from.max(to), w + 4)),
                Item::Note {
                    place: Place::Right(p),
                    ..
                } if p + 1 < count => needs.push((p, p + 1, w + 7)),
                Item::Note {
                    place: Place::Left(p),
                    ..
                } if p > 0 => needs.push((p - 1, p, w + 7)),
                Item::Note {
                    place: Place::Over(a, b),
                    ..
                } => {
                    let half = i64::midpoint(w, 4) + 2;
                    if a == b {
                        if a > 0 {
                            needs.push((a - 1, a, half));
                        }
                        if a + 1 < count {
                            needs.push((a, a + 1, half + 1));
                        }
                    } else {
                        needs.push((a, b, w - 1));
                    }
                }
                _ => {}
            }
        }
        needs
    }

    /// Lifeline columns: as close as the needs allow, then widened until
    /// every frame clears the lifelines outside it.
    fn columns(&mut self) {
        let count = self.diagram.participants.len();
        let mut gaps = vec![1; count.saturating_sub(1)];
        let mut needs = self.needs();
        needs.sort_by_key(|&(a, b, _)| b - a);
        for (a, b, d) in needs {
            let short = d - gaps[a..b].iter().sum::<i64>();
            if short > 0 {
                let n = int(b - a);
                for (k, gap) in gaps[a..b].iter_mut().enumerate() {
                    *gap += short / n + i64::from(int(k) < short % n);
                }
            }
        }
        for _ in 0..WIDENINGS {
            self.xs = std::iter::once(0)
                .chain(gaps.iter().scan(0, |x, g| {
                    *x += g;
                    Some(*x)
                }))
                .collect();
            self.frames = self.frame_boxes();
            let mut widened = false;
            for frame in &self.frames {
                if frame.first > 0 && self.xs[frame.first - 1] >= frame.lo - 1 {
                    gaps[frame.first - 1] += self.xs[frame.first - 1] - frame.lo + 2;
                    widened = true;
                }
                if frame.last + 1 < count && self.xs[frame.last + 1] <= frame.hi + 1 {
                    gaps[frame.last] += frame.hi + 2 - self.xs[frame.last + 1];
                    widened = true;
                }
            }
            if !widened {
                break;
            }
        }
    }

    /// A note's left and right columns.
    fn note_box(&self, k: usize, place: &Place) -> (i64, i64) {
        let w = self.widest(k) + 4;
        match *place {
            Place::Right(p) => (self.xs[p] + 2, self.xs[p] + 1 + w),
            Place::Left(p) => (self.xs[p] - 1 - w, self.xs[p] - 2),
            Place::Over(a, b) => {
                let w = w.max(self.xs[b] - self.xs[a] + 5);
                let left = i64::midpoint(self.xs[a], self.xs[b]) - w / 2;
                (left, left + w - 1)
            }
        }
    }

    fn extent(&self, k: usize) -> Option<Extent> {
        match &self.diagram.items[k].1 {
            &Item::Message { from, to, .. } if from == to => Some((
                self.xs[from],
                self.xs[from] + 5 + self.widest(k),
                from,
                from,
            )),
            &Item::Message { from, to, .. } => {
                let (a, b) = (from.min(to), from.max(to));
                Some((self.xs[a], self.xs[b], a, b))
            }
            Item::Note { place, .. } => {
                let (lo, hi) = self.note_box(k, place);
                let (a, b) = match *place {
                    Place::Left(p) | Place::Right(p) => (p, p),
                    Place::Over(a, b) => (a, b),
                };
                Some((lo, hi, a, b))
            }
            _ => None,
        }
    }

    fn frame_boxes(&self) -> Vec<FrameBox> {
        let last = self.diagram.participants.len() - 1;
        let mut frames = Vec::new();
        // Per open frame: its `Open`, the widest title or divider, and
        // the extent of what's inside so far.
        let mut open: Vec<(usize, i64, Option<Extent>)> = Vec::new();
        let merge = |a: Option<Extent>, b: Extent| {
            Some(a.map_or(b, |a| {
                (a.0.min(b.0), a.1.max(b.1), a.2.min(b.2), a.3.max(b.3))
            }))
        };
        for (k, (_, item)) in self.diagram.items.iter().enumerate() {
            match item {
                Item::Open { keyword, label } | Item::Divider { keyword, label } => {
                    let title = int(canvas::width(keyword) + canvas::width(label)) + 1;
                    if matches!(item, Item::Open { .. }) {
                        open.push((k, title, None));
                    } else if let Some(top) = open.last_mut() {
                        top.1 = top.1.max(title);
                    }
                }
                Item::Close => {
                    let Some((start, title, inside)) = open.pop() else {
                        continue;
                    };
                    let (lo, hi, first, last) =
                        inside.unwrap_or((self.xs[0], self.xs[last], 0, last));
                    let (lo, hi) = (lo - 2, hi + 2);
                    let frame = FrameBox {
                        open: start,
                        close: k,
                        lo,
                        hi: hi.max(lo + title + 5),
                        first,
                        last,
                    };
                    if let Some(parent) = open.last_mut() {
                        parent.2 = merge(parent.2, (frame.lo, frame.hi, first, last));
                    }
                    frames.push(frame);
                }
                _ => {
                    if let (Some(top), Some(extent)) = (open.last_mut(), self.extent(k)) {
                        top.2 = merge(top.2, extent);
                    }
                }
            }
        }
        frames
    }

    /// Each item's rows, after the participants' three.
    fn stack(&mut self) {
        let mut at = 3;
        for (k, (_, item)) in self.diagram.items.iter().enumerate() {
            let lines = self.texts[k].len().max(1);
            let count = match item {
                Item::Message { from, to, .. } if from == to => lines + 2,
                Item::Message { .. } => lines + 1,
                Item::Note { .. } => lines + 3,
                _ => 1,
            };
            self.rows.push((at, count));
            at += count;
        }
    }

    fn draw(&self) -> Vec<Block> {
        let offset = -self.leftmost();
        let x = |v: i64| cell(v + offset);
        let height = self.rows.last().map_or(3, |&(at, n)| at + n);
        let mut canvas = Canvas::default();
        canvas.grow(height);
        for (p, participant) in self.diagram.participants.iter().enumerate() {
            let w = self.box_width(p);
            let left = self.xs[p] - w / 2;
            let (a, b) = ((x(left), 0), (x(left + w - 1), 2));
            canvas.rect(a, b, BORDER);
            if participant.actor {
                for (spot, c) in [(a, '╭'), ((b.0, 0), '╮'), ((a.0, 2), '╰'), (b, '╯')] {
                    canvas.put(spot, c, BORDER);
                }
            }
            canvas.text((a.0 + 2, 1), &participant.label, TITLE);
            // A row past the end, which no block takes: the last row's
            // lifelines then look as they do above, and appending a
            // statement leaves the last block as it was.
            canvas.line(
                (x(self.xs[p]), 2),
                (x(self.xs[p]), height),
                Stroke::Light,
                BORDER,
            );
        }
        for frame in &self.frames {
            self.frame(&mut canvas, frame, offset);
        }
        for (k, (_, item)) in self.diagram.items.iter().enumerate() {
            match item {
                Item::Message { .. } => self.message(&mut canvas, k, offset),
                Item::Note { place, .. } => self.note(&mut canvas, k, place, offset),
                _ => {}
            }
        }
        let mut lines = canvas.lines().into_iter();
        let mut blocks = vec![Block {
            kind: "participants",
            source: self.diagram.header.clone(),
            lines: lines.by_ref().take(3).collect(),
        }];
        for (k, &(line, ref item)) in self.diagram.items.iter().enumerate() {
            blocks.push(Block {
                kind: match item {
                    Item::Message { .. } => "message",
                    Item::Note { .. } => "note",
                    _ => "frame",
                },
                source: line..line + 1,
                lines: lines.by_ref().take(self.rows[k].1).collect(),
            });
        }
        blocks
    }

    /// The least column anything is drawn at.
    fn leftmost(&self) -> i64 {
        let boxes = (0..self.xs.len()).map(|p| self.xs[p] - self.box_width(p) / 2);
        let frames = self.frames.iter().map(|f| f.lo);
        let notes = (0..self.diagram.items.len()).filter_map(|k| self.extent(k).map(|e| e.0));
        boxes.chain(frames).chain(notes).min().unwrap_or(0)
    }

    /// The drawing's width and height, without drawing it.
    fn size(&self) -> (usize, usize) {
        let boxes = (0..self.xs.len()).map(|p| {
            let w = self.box_width(p);
            self.xs[p] - w / 2 + w - 1
        });
        let frames = self.frames.iter().map(|f| f.hi);
        let items = (0..self.diagram.items.len()).filter_map(|k| self.extent(k).map(|e| e.1));
        let rightmost = boxes.chain(frames).chain(items).max().unwrap_or(0);
        let height = self.rows.last().map_or(3, |&(at, n)| at + n);
        (cell(rightmost - self.leftmost() + 1), height)
    }

    fn frame(&self, canvas: &mut Canvas, frame: &FrameBox, offset: i64) {
        let (lo, hi) = (cell(frame.lo + offset), cell(frame.hi + offset));
        let (top, bottom) = (self.rows[frame.open].0, self.rows[frame.close].0);
        canvas.rect((lo, top), (hi, bottom), BORDER);
        for k in frame.open..frame.close {
            let nested = || {
                self.frames
                    .iter()
                    .any(|f| f.open > frame.open && f.open < k && k < f.close)
            };
            let row = self.rows[k].0;
            let (keyword, label) = match &self.diagram.items[k].1 {
                Item::Open { keyword, label } if k == frame.open => (keyword, label),
                Item::Divider { keyword, label } if !nested() => {
                    canvas.line((lo, row), (hi, row), Stroke::Dotted, BORDER);
                    (keyword, label)
                }
                _ => continue,
            };
            let mut at = lo + 2;
            at += canvas.text((at, row), " ", PLAIN);
            at += canvas.text((at, row), keyword, KEYWORD);
            let rest = if label.is_empty() {
                " ".to_owned()
            } else {
                format!(" {label} ")
            };
            canvas.text((at, row), &rest, PLAIN);
        }
    }

    fn message(&self, canvas: &mut Canvas, k: usize, offset: i64) {
        let Item::Message {
            from,
            to,
            stroke,
            tip,
            both,
            number,
            ..
        } = self.diagram.items[k].1
        else {
            return;
        };
        let (start, _) = self.rows[k];
        let lines = &self.texts[k];
        let number = number.map(|n| format!("{n} "));
        let (a, b) = (self.xs[from] + offset, self.xs[to] + offset);
        if from == to {
            let (out, back) = (start + 1, start + lines.len().max(1) + 1);
            let x = cell(a);
            canvas.line((x, out), (x + 3, out), stroke, EDGE);
            canvas.line((x + 3, out), (x + 3, back), stroke, EDGE);
            let end = if tip == Tip::None { x } else { x + 1 };
            canvas.line((x + 3, back), (end, back), stroke, EDGE);
            if tip != Tip::None {
                canvas.put((x + 1, back), head(tip, false), EDGE);
            }
            text(canvas, (x + 5, out), number.as_deref(), lines);
            return;
        }
        let w = self.widest(k);
        let left = a.min(b) + (a.max(b) - a.min(b) - w + 1) / 2;
        text(canvas, (cell(left), start), number.as_deref(), lines);
        let row = start + lines.len().max(1);
        let right = b > a;
        let step = if right { 1 } else { -1 };
        let tail = if both { a + step } else { a };
        let end = if tip == Tip::None { b } else { b - step };
        canvas.line((cell(tail), row), (cell(end), row), stroke, EDGE);
        if tip != Tip::None {
            canvas.put((cell(b - step), row), head(tip, right), EDGE);
        }
        if both {
            canvas.put((cell(a + step), row), head(tip, !right), EDGE);
        }
    }

    fn note(&self, canvas: &mut Canvas, k: usize, place: &Place, offset: i64) {
        let (lo, hi) = self.note_box(k, place);
        let (start, count) = self.rows[k];
        let (a, b) = (
            (cell(lo + offset), start + 1),
            (cell(hi + offset), start + count - 1),
        );
        canvas.clear(a, b);
        canvas.rect(a, b, BORDER);
        for (j, line) in self.texts[k].iter().enumerate() {
            canvas.text((a.0 + 2, a.1 + 1 + j), line, PLAIN);
        }
    }
}

/// A message's text lines from `at` down, the first led by its number.
fn text(canvas: &mut Canvas, (x, y): (usize, usize), number: Option<&str>, lines: &[String]) {
    let indent = number.map_or(0, canvas::width);
    if let Some(number) = number {
        canvas.text((x, y), number, NUMBER);
    }
    for (j, line) in lines.iter().enumerate() {
        canvas.text((x + indent, y + j), line, PLAIN);
    }
}

fn head(tip: Tip, right: bool) -> char {
    match (tip, right) {
        (Tip::Open, true) => '▷',
        (Tip::Open, false) => '◁',
        (Tip::Cross, _) => '×',
        (_, true) => '▶',
        (_, false) => '◀',
    }
}

#[cfg(test)]
mod tests;

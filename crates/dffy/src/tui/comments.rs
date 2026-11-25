//! Review comments drawn in the viewer: a thread's box, and the comments
//! panel over the list [`crate::review::listing`] keeps. The threads are
//! [`crate::review::model`]'s.
//!
//! The diff draws a placed thread under its last line, a comment on the
//! whole file under the file's first row, and one whose lines are gone
//! under its last row; [`thread_rows`] writes what goes there.

use std::borrow::Cow;
use std::ops::Range;

use dffy_core::document::Side;
use dffy_core::view::{Attributes, Syntax};

use super::markdown::{self, Body};
use super::textbox::TextBox;
use super::tree;
use super::view::paint;
use crate::columns::Columns as _;
use crate::keys::{Action as Act, Keys};
use crate::render::{Canvas, escape, plain};
use crate::review::listing::{Absent, List, Listing, Row};
use crate::review::model::text::{age, epoch};
use crate::review::model::{
    self, At, Comments, Look, Posting, Snippet, Standing, Thread, ThreadComment,
};
use crate::review::who;
use crate::term::{Grid, Key, Mods};
use crate::theme::{Ink, Role, Theme};

/// The text box's keys, in its heading.
const BOX_KEYS: &str = " · enter saves · alt+enter new line · ctrl+g editor";

/// The panel's key listing the threads on other branches, or folding them.
pub(super) const OTHER_BRANCHES: char = 'b';

/// The text box open in the diff, and what its text becomes.
#[derive(Clone)]
pub(crate) struct Writing {
    pub at: Writes,
    pub text: TextBox,
    /// What `enter` does with a new thread, for its box's heading.
    pub enter: Enter,
}

/// What `enter` does with a new thread's text box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Enter {
    Saves,
    /// Saves it and asks this agent about it; `None` before the agent
    /// plugin named its default one.
    Asks(Option<String>),
    /// Saves it and hands its thread to your coding agent: the session it
    /// names, safe to draw, or with none a new one.
    Fixes(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Writes {
    /// A new thread on `lines` (0-based, in the line diff) of `side`.
    New { side: Side, lines: Range<usize> },
    /// A new thread on the whole file shown, under its first row.
    File,
    /// A reply, at the end of the thread with this root.
    Reply(String),
    /// Comment `id` of the thread with root `root`, in place of its text.
    Edit { root: String, id: String },
}

/// The mark a thread that looks `look` shows by. Shapes differ as well as
/// colors: without color the theme is attributes only.
pub(crate) fn glyph(look: Look) -> &'static str {
    match look {
        Look::Resolved => "○",
        Look::Open => "●",
        Look::Draft => "◆",
    }
}

pub(crate) fn look_role(look: Look) -> Role {
    match look {
        Look::Resolved => Role::Dim,
        Look::Open => Role::Hint,
        Look::Draft => Role::Warn,
    }
}

fn posting_role(posting: &Posting) -> Role {
    if posting.warn { Role::Warn } else { Role::Dim }
}

/// When [`age`] next says something else of `created`, from `now`;
/// `u64::MAX` once it's a date.
fn age_changes(created: &str, now: u64) -> u64 {
    epoch(created).map_or(u64::MAX, |then| ago_changes(then, now))
}

/// When [`crate::review::model::text::ago`] next says something else of
/// `then`, from `now`, both in seconds since 1970; `u64::MAX` once it says
/// nothing.
pub(crate) fn ago_changes(then: u64, now: u64) -> u64 {
    let step = match now.saturating_sub(then) {
        s if s < 3600 => 60,
        s if s < 86_400 => 3600,
        s if s < 30 * 86_400 => 86_400,
        _ => return u64::MAX,
    };
    then + (now.saturating_sub(then) / step + 1) * step
}

/// When an age `thread`'s box shows next reads differently, from `now`.
pub(crate) fn ages_change(thread: &Thread, now: u64) -> u64 {
    (thread.comments.iter())
        .map(|n| age_changes(&n.created, now))
        .min()
        .unwrap_or(u64::MAX)
}

const AUTHOR: Ink = Ink {
    role: Role::Plain,
    syntax: None,
    attributes: Attributes::BOLD,
};

const DIM: Ink = Ink {
    role: Role::Dim,
    syntax: None,
    attributes: Attributes::NONE,
};

fn spans_width<I>(spans: &[(I, &str)]) -> usize {
    spans.iter().map(|(_, t)| t.width()).sum()
}

/// Pushes `spans`, cut to `width` columns with `…` when they're longer.
/// Returns the columns they took.
fn cut<I: Into<Ink> + Copy>(c: &mut Canvas, spans: &[(I, &str)], width: usize) -> usize {
    let total = spans_width(spans);
    if total <= width {
        for (ink, text) in spans {
            c.push_ink((*ink).into(), text);
        }
        return total;
    }
    if width == 0 {
        return 0;
    }
    let mut used = 0;
    for (ink, text) in spans {
        let room = width - 1 - used;
        if text.width() <= room {
            c.push_ink((*ink).into(), text);
            used += text.width();
            continue;
        }
        let mut cut = String::new();
        for (_, piece, w) in text.clusters() {
            if used + w > width - 1 {
                break;
            }
            used += w;
            cut.push_str(piece);
        }
        c.push_ink((*ink).into(), &cut);
        break;
    }
    c.push(Role::Dim, "…");
    used + 1
}

/// Pushes `width` columns: `left` cut to fit, and `right` at the right end
/// when that leaves a dozen columns of `left`.
fn spread(c: &mut Canvas, left: &[(Ink, &str)], right: &[(Role, &str)], width: usize) {
    let right_width = match spans_width(right) {
        w if w > 0 && w + 2 + 12 <= width => w,
        _ => 0,
    };
    let room = match right_width {
        0 => width,
        w => width - w - 2,
    };
    let used = cut(c, left, room);
    c.spaces(Role::Plain, width - used - right_width);
    if right_width > 0 {
        cut(c, right, right_width);
    }
}

/// The lines of a thread's box: light, heavy while it has the focus, and
/// with round corners while it holds a draft. The corners tell an open
/// draft from other threads without color, as the theme is attributes
/// only; focused, or folded to a card without corners, only the role
/// does. Lines stay solid: dashed ones break up into dots in some fonts.
#[derive(Clone, Copy)]
struct Border {
    role: Role,
    /// Top left, top right, bottom left, bottom right.
    corners: [&'static str; 4],
    across: &'static str,
    down: &'static str,
}

impl Border {
    fn new(draft: bool, focused: bool) -> Self {
        let role = match (draft, focused) {
            (true, _) => Role::Warn,
            (false, true) => Role::Hint,
            (false, false) => Role::Dim,
        };
        let (across, down) = if focused {
            ("━", "┃")
        } else {
            ("─", "│")
        };
        let corners = match (draft, focused) {
            (_, true) => ["┏", "┓", "┗", "┛"],
            (true, false) => ["╭", "╮", "╰", "╯"],
            (false, false) => ["┌", "┐", "└", "┘"],
        };
        Self {
            role,
            corners,
            across,
            down,
        }
    }
}

/// Writes a box's rows into a canvas, each exactly `width` columns, with a
/// cell of padding inside the sides.
struct Frame<'a> {
    c: &'a mut Canvas,
    border: Border,
    width: usize,
}

/// Columns of text inside the padding of a box `width` columns wide.
pub(crate) fn inner(width: usize) -> usize {
    width.saturating_sub(4).max(1)
}

/// Columns from a box's left edge to its text: the side and its padding.
pub(crate) const TEXT_COLUMN: usize = 2;

/// The text of each of `thread`'s comments rendered for its box, `width`
/// columns wide: what [`thread_rows`] draws open. `highlight` colors their
/// fenced code.
pub(crate) fn bodies(thread: &Thread, width: usize, highlight: bool) -> Vec<Body> {
    thread
        .comments
        .iter()
        .map(|comment| markdown::render(&comment.text, inner(width), highlight))
        .collect()
}

impl Frame<'_> {
    fn inner(&self) -> usize {
        inner(self.width)
    }

    /// A row of the border between `ends`, carrying `spans` after the
    /// first when it has room for them.
    fn edge(&mut self, ends: [&str; 2], spans: &[(Ink, &str)]) {
        let Border { role, across, .. } = self.border;
        let room = self.width.saturating_sub(2);
        self.c.push(role, ends[0]);
        let mut used = 0;
        if !spans.is_empty() && room >= 3 {
            self.c.push(Role::Plain, " ");
            used += 2 + cut(self.c, spans, room - 2);
            self.c.push(Role::Plain, " ");
        }
        self.c.push(role, &across.repeat(room.saturating_sub(used)));
        self.c.push(role, ends[1]);
        self.c.end_line();
    }

    /// A folded thread's one row, between the box's sides.
    fn card(&mut self, left: &[(Ink, &str)], right: &[(Role, &str)]) {
        let Border { role, down, .. } = self.border;
        let inner = self.inner();
        self.c.push(role, down);
        self.c.push(Role::Plain, " ");
        spread(self.c, left, right, inner);
        self.c.push(Role::Plain, " ");
        self.c.push(role, down);
        self.c.end_line();
    }

    fn top(&mut self, spans: &[(Ink, &str)]) {
        let [left, right, ..] = self.border.corners;
        self.edge([left, right], spans);
    }

    /// A reply's heading, under a blank row parting it from the comment
    /// above.
    fn heading(&mut self, spans: &[(Ink, &str)]) {
        self.body::<Role>(&[]);
        self.body(spans);
    }

    fn bottom(&mut self) {
        let [.., left, right] = self.border.corners;
        self.edge([left, right], &[]);
    }

    /// A row of text inside the box, cut to fit.
    fn body<I: Into<Ink> + Copy>(&mut self, spans: &[(I, &str)]) {
        let Border { role, down, .. } = self.border;
        let inner = self.inner();
        self.c.push(role, down);
        self.c.push(Role::Plain, " ");
        let used = cut(self.c, spans, inner);
        self.c.spaces(Role::Plain, inner - used + 1);
        self.c.push(role, down);
        self.c.end_line();
    }

    /// A snippet's code, numbered, each line cut to fit as a suggestion's
    /// are, then a blank row parting it from the comment.
    fn code(&mut self, snippet: &Snippet) {
        let Some(last) = snippet.rows.iter().map(|r| r.number).max() else {
            return;
        };
        let digits = last.to_string().len();
        for row in &snippet.rows {
            let pieces: Vec<(Option<Syntax>, String)> = (row.pieces.iter())
                .map(|(syntax, text)| (*syntax, plain(text)))
                .collect();
            let number = format!("{:>digits$} ", row.number);
            let sign = match row.sign {
                "+" => Role::Added,
                "-" => Role::Removed,
                _ => Role::Dim,
            };
            let mut spans: Vec<(Ink, &str)> = vec![
                (Role::Dim.into(), &number),
                (sign.into(), row.sign),
                (Role::Plain.into(), " "),
            ];
            let role = if row.context { Role::Dim } else { Role::Plain };
            spans.extend(pieces.iter().map(|(syntax, text)| {
                let ink = Ink {
                    syntax: *syntax,
                    ..role.into()
                };
                (ink, text.as_str())
            }));
            self.body(&spans);
        }
        self.body::<Role>(&[]);
    }

    /// A suggested change: the commented `lines` it replaces, then its own.
    /// Once applied, the thread's lines are its own, so only those show,
    /// dimmed.
    fn suggestion(&mut self, lines: &[String], suggestion: &str, applied: bool) {
        if applied {
            self.body(&[(Role::Dim, "suggested change, applied:")]);
            for line in suggestion.lines() {
                self.body(&[(Role::Dim, "  "), (Role::Dim, &plain(line))]);
            }
            return;
        }
        self.body(&[(Role::Dim, "suggested change:")]);
        let old = lines.iter().map(|l| (Role::Removed, "- ", l.as_str()));
        let new = suggestion.lines().map(|l| (Role::Added, "+ ", l));
        for (role, sign, text) in old.chain(new) {
            self.body(&[(role, sign), (role, &plain(text))]);
        }
    }

    /// The text box's wrapped rows, its cursor a bar in the text.
    fn text_box(&mut self, text: &mut TextBox) {
        // A column for the cursor.
        let room = self.inner().saturating_sub(1).max(1);
        for (line, cursor) in text.rows(room) {
            match cursor {
                Some(at) => self.body(&[
                    (Role::Plain, &line[..at]),
                    (Role::Hint, "▏"),
                    (Role::Plain, &line[at..]),
                ]),
                None => self.body(&[(Role::Plain, &line)]),
            }
        }
    }
}

/// The model's word for where a comment lives, in the role it's drawn in.
fn standing_tag(standing: Standing) -> Option<(Role, &'static str)> {
    let role = if standing == Standing::Draft {
        Role::Warn
    } else {
        Role::Dim
    };
    standing.word().map(|word| (role, word))
}

/// The model's word for where a thread's first comment lives, on its row
/// in the comments panel, where the gutter's mark says a draft.
fn row_word(standing: Standing) -> Option<&'static str> {
    standing.word().filter(|_| standing != Standing::Draft)
}

/// The words after a comment's author: its label, as an agent's mark,
/// where it lives, with `share`, the words for the key that makes a
/// private one a draft (`A shares`), what posting it does, whether it's on
/// the host for you only, and whether its suggestion is applied.
fn comment_tags<'a>(
    comment: &'a ThreadComment,
    share: Option<&'a str>,
) -> impl Iterator<Item = (Role, Cow<'a, str>)> {
    let shares = share.filter(|_| comment.standing == Standing::Unposted);
    (comment.label.as_deref())
        .map(|label| (Role::Dim, Cow::Owned(escape(label.as_bytes()))))
        .into_iter()
        .chain(standing_tag(comment.standing).map(|(role, tag)| (role, Cow::Borrowed(tag))))
        .chain(shares.map(|words| (Role::Hint, Cow::Borrowed(words))))
        .chain(comment.posting.as_ref().map(|posting| {
            let words = escape(posting.words.as_bytes());
            (posting_role(posting), Cow::Owned(words))
        }))
        .chain(
            comment
                .pending
                .then_some((Role::Hint, Cow::Borrowed("pending"))),
        )
        .chain(
            comment
                .applied
                .then_some((Role::Dim, Cow::Borrowed("applied"))),
        )
}

/// The words after a thread's first author: its state, then where it is
/// when that's not on a line, with the line it was on.
fn tags<'a>(
    thread: &'a Thread,
    first: &'a ThreadComment,
    share: Option<&'a str>,
) -> Vec<(Role, Cow<'a, str>)> {
    let mut out: Vec<(Role, Cow<'a, str>)> = comment_tags(first, share).collect();
    let mut tag = |role: Role, tag: &'a str| out.push((role, Cow::Borrowed(tag)));
    if !first.applied && thread.comments.iter().any(|c| c.applied) {
        tag(Role::Dim, "applied");
    }
    if thread.local {
        tag(Role::Hint, "branch draft");
    }
    if thread.resolved {
        tag(Role::Dim, "resolved");
    }
    if thread.outdated {
        tag(Role::Warn, "outdated");
    }
    match (&thread.at, &thread.snippet) {
        (At::File, _) => tag(Role::Dim, "on the file"),
        (At::Lost, Some(snippet)) => tag(Role::Warn, &snippet.heading),
        (At::Lost, _) => tag(Role::Warn, "lines not found"),
        (At::Conversation { kind, label }, _) => {
            if let Some(kind) = shown_kind(kind.as_deref()) {
                tag(Role::Dim, kind);
            }
            if let Some(label) = label {
                out.push((Role::Hint, Cow::Owned(escape(label.as_bytes()))));
            }
        }
        (At::Line { .. }, _) => {}
    }
    out
}

/// The word a general comment's `kind` shows as; none for a plain comment
/// and for a kind dffy doesn't know, which shows as one.
fn shown_kind(kind: Option<&str>) -> Option<&'static str> {
    match kind? {
        "description" => Some("description"),
        "review" => Some("review"),
        _ => None,
    }
}

/// Where a comment is in its thread's open box, in rows from the box's top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommentRows {
    /// Its heading's: the top border for the first comment.
    pub first: usize,
    /// Its rendered text's; none while the text box edits it.
    pub text: Range<usize>,
}

/// Writes `thread`'s box, `width` columns wide. Collapsed, one row between
/// the box's sides sums it up, and it returns true: inks carry no
/// background, so the caller tints that row. Open, it draws `bodies`, its
/// [`bodies`] at `width`.
/// `focused` draws the border heavy. `writing`, when the text box is in
/// this thread, takes the place of the comment it edits or follows the
/// last as a reply. `now` is seconds since 1970. `share` follows the word
/// of a comment kept private, saying the key that makes it a draft. Open,
/// `comment_rows` gets where each comment is in the box.
pub(crate) fn thread_rows(
    thread: &Thread,
    bodies: &[Body],
    (collapsed, focused): (bool, bool),
    mut writing: Option<&mut Writing>,
    (width, now, share): (usize, u64, Option<&str>),
    (c, comment_rows): (&mut Canvas, &mut Vec<CommentRows>),
) -> bool {
    comment_rows.clear();
    let Some(first) = thread.comments.first() else {
        return false;
    };
    let border = Border::new(thread.look == Look::Draft, focused);
    let top = c.len();
    let mut frame = Frame { c, border, width };
    if collapsed && writing.is_none() {
        let author = escape(who(&first.author).as_bytes());
        let when = format!(" · {}", escape(age(&first.created, now).as_bytes()));
        let replies = match thread.comments.len() - 1 {
            0 => String::new(),
            1 => "1 reply".to_owned(),
            n => format!("{n} replies"),
        };
        let tags = tags(thread, first, share);
        let mut spans = vec![
            (Role::Dim.into(), "▸ "),
            (AUTHOR, author.as_str()),
            (Role::Dim.into(), when.as_str()),
        ];
        for (role, tag) in &tags {
            spans.extend([(Role::Dim.into(), " · "), ((*role).into(), &**tag)]);
        }
        spans.extend([
            (Role::Dim.into(), " · "),
            (Role::Plain.into(), first.summary.as_str()),
        ]);
        frame.card(&spans, &[(Role::Dim, &replies)]);
        return true;
    }
    for (i, comment) in thread.comments.iter().enumerate() {
        let author = escape(who(&comment.author).as_bytes());
        let when = format!(" · {}", escape(age(&comment.created, now).as_bytes()));
        let words = if i == 0 {
            tags(thread, comment, share)
        } else {
            comment_tags(comment, share).collect()
        };
        let mut spans = if i == 0 {
            vec![(Role::Dim.into(), "▾ ")]
        } else {
            Vec::new()
        };
        spans.extend([(AUTHOR, author.as_str()), (Role::Dim.into(), when.as_str())]);
        for (role, word) in &words {
            spans.extend([(Role::Dim.into(), " · "), ((*role).into(), &**word)]);
        }
        let editing = writing
            .as_deref_mut()
            .filter(|w| matches!(&w.at, Writes::Edit { id, .. } if *id == comment.id));
        if editing.is_some() {
            spans.push((Role::Dim.into(), BOX_KEYS));
        }
        // A reply's heading is under a blank row.
        let heading = frame.c.len() - top + usize::from(i > 0);
        if i == 0 {
            frame.top(&spans);
            if let Some(snippet) = &thread.snippet {
                frame.code(snippet);
            }
        } else {
            frame.heading(&spans);
        }
        let from = frame.c.len() - top;
        if let Some(writing) = editing {
            frame.text_box(&mut writing.text);
            comment_rows.push(CommentRows {
                first: heading,
                text: from..from,
            });
            continue;
        }
        for row in bodies.get(i).iter().flat_map(|body| body.rows.rows()) {
            frame.body(&row.collect::<Vec<_>>());
        }
        comment_rows.push(CommentRows {
            first: heading,
            text: from..frame.c.len() - top,
        });
        if let Some(suggestion) = &comment.suggestion {
            frame.suggestion(&thread.lines, suggestion, comment.applied);
        }
    }
    if let Some(writing) = writing.filter(|w| matches!(w.at, Writes::Reply(_))) {
        let dim = Role::Dim.into();
        frame.heading(&[(AUTHOR, "you"), (dim, " · reply"), (dim, BOX_KEYS)]);
        frame.text_box(&mut writing.text);
    }
    frame.bottom();
    false
}

/// Writes the text box for a new thread, `width` columns wide: a draft's
/// box with the focus. Its heading in the top border says what it writes,
/// the lines it's on and what `enter` does, then the other keys; too
/// narrow for all of it, the keys go from the last, then the lines, then
/// the agent a question is for, then what it writes.
pub(crate) fn new_rows(writing: &mut Writing, width: usize, c: &mut Canvas) {
    let what = match &writing.enter {
        Enter::Saves => Cow::Borrowed(" · new comment"),
        Enter::Asks(agent) => {
            let agent = agent.as_deref().map_or(Cow::Borrowed("the agent"), |a| {
                Cow::Owned(escape(a.as_bytes()))
            });
            Cow::Owned(format!(" · question for {agent}"))
        }
        Enter::Fixes(None) => Cow::Borrowed(" · fix with agent"),
        Enter::Fixes(Some(session)) => Cow::Owned(format!(" · for {session}")),
    };
    let lines = match &writing.at {
        Writes::New { lines, .. } if lines.len() > 1 => {
            format!(" · lines {}–{}", lines.start + 1, lines.end)
        }
        Writes::New { lines, .. } => format!(" · line {}", lines.start + 1),
        Writes::File => " · whole file".to_owned(),
        Writes::Reply(_) | Writes::Edit { .. } => String::new(),
    };
    let enter = match &writing.enter {
        Enter::Saves => " · enter saves",
        Enter::Asks(_) => " · enter asks",
        Enter::Fixes(_) => " · enter sends it",
    };
    let dim: Ink = Role::Dim.into();
    let mut spans = vec![
        (dim, "▾ "),
        (AUTHOR, "you"),
        (dim, &*what),
        (dim, &*lines),
        (dim, enter),
        (dim, " · alt+enter new line"),
        (dim, " · ctrl+g editor"),
    ];
    let mut question = matches!(writing.enter, Enter::Asks(_)).then_some(" · question");
    // The top border's room for a heading: inside its corners, a space
    // either side.
    while spans.len() > 2 && spans_width(&spans) > width.saturating_sub(4) {
        match (spans.len(), question) {
            (3, Some(short)) => {
                spans[2].1 = short;
                question = None;
            }
            _ => drop(spans.pop()),
        }
    }
    let mut frame = Frame {
        c,
        border: Border::new(true, true),
        width,
    };
    frame.top(&spans);
    frame.text_box(&mut writing.text);
    frame.bottom();
}

/// What a key in the comments panel did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Stay,
    /// The selection moved to the thread with this root: the diff shows it.
    Moved(String),
    /// The keys go to the diff.
    Diff,
    /// The keys go to the diff, on the selected thread's file, which is
    /// opened when the diff doesn't list it.
    Open,
    /// Not the panel's: the key does what it does in the tree.
    Pass,
}

/// A thread's row narrower than this leaves out its line, and narrower
/// than [`REPLIES_COLUMNS`] its replies too.
const LINE_COLUMNS: usize = 23;
const REPLIES_COLUMNS: usize = 19;

/// The comment threads listed by file in the left panel, in place of the
/// tree (`i`). Moving the selection shows the thread in the diff, which is
/// its preview: see [`Outcome::Moved`]. What's listed and selected is the
/// [`List`]'s; the panel maps keys to it, scrolls and draws the rows on
/// screen.
pub(super) struct Panel {
    list: List,
    /// A file the diff doesn't list opens from the working tree.
    opens: bool,
    /// Keys go to the search.
    pub editing: bool,
    /// The selected thread's row, when there's one.
    selected: usize,
    /// The last root [`Panel::select`] didn't find in the rows, so a frame
    /// with a thread focused that the filter hides doesn't look again.
    missed: Option<String>,
    scroll: usize,
    /// Rows the panel showed last, for paging and taps.
    height: usize,
}

impl Panel {
    /// `listing` while the list of files is still growing; with `opens`, a
    /// file the diff doesn't list opens from the working tree. The
    /// selection starts on the thread with root `near.0`, else on the first
    /// of file `near.1`'s, else on the first.
    pub(super) fn new(
        comments: &Comments,
        (listing, opens): (bool, bool),
        near: (Option<&str>, &[u8]),
    ) -> Self {
        let list = List::new(comments, listing);
        let selected = list.near(near).unwrap_or_else(|| list.keep(None, 0));
        Self {
            list,
            opens,
            editing: false,
            selected,
            missed: None,
            scroll: 0,
            height: 1,
        }
    }

    /// The selected thread's root.
    pub(super) fn selected(&self) -> Option<&str> {
        self.thread().map(|t| t.root.as_str())
    }

    /// The selected thread, as the list was last made: placed, or made
    /// for the panel when its file isn't in the diff.
    pub(super) fn thread(&self) -> Option<&Thread> {
        self.list.thread_at(self.selected)
    }

    pub(super) fn select(&mut self, root: &str) {
        if self.selected() == Some(root) || self.missed.as_deref() == Some(root) {
            return;
        }
        match self.list.row_of(root) {
            Some(row) => self.selected = row,
            None => self.missed = Some(root.to_owned()),
        }
    }

    /// The wheel moves the selection a thread a notch. Returns the root it
    /// moved to.
    pub(super) fn scroll_by(&mut self, by: isize) -> Option<String> {
        let before = self.selected().map(str::to_owned);
        self.selected = self.list.step(self.selected, by);
        self.selected()
            .filter(|&r| before.as_deref() != Some(r))
            .map(str::to_owned)
    }

    /// Selects the thread at panel line `y` (0 is the first row below the
    /// border), and returns its root.
    pub(super) fn click(&mut self, y: usize) -> Option<String> {
        let row = self.scroll + y;
        if y >= self.height || self.list.thread_at(row).is_none() {
            return None;
        }
        self.selected = row;
        self.selected().map(str::to_owned)
    }

    /// Makes the list again when it's out of date, keeping the selected
    /// thread when it's still listed, else the next one.
    fn sync(&mut self, comments: &Comments, listing: bool) {
        let root = self.selected().map(str::to_owned);
        if self.list.sync(comments, listing) {
            self.missed = None;
            self.selected = self.list.keep(root.as_deref(), self.selected);
        }
    }

    /// What `key` does; `listing` while the list of files is still
    /// growing.
    pub(super) fn key(
        &mut self,
        (comments, listing): (&Comments, bool),
        keys: &Keys,
        key: Key,
        mods: Mods,
    ) -> Outcome {
        self.sync(comments, listing);
        let before = self.selected().map(str::to_owned);
        let outcome = self.act(keys, key, mods);
        // A filter or search dropping the selected thread moves it too.
        self.sync(comments, listing);
        match (outcome, self.selected()) {
            (Outcome::Stay, Some(root)) if before.as_deref() != Some(root) => {
                Outcome::Moved(root.to_owned())
            }
            (outcome, _) => outcome,
        }
    }

    /// While the search is typed, keys with ctrl still move and quit.
    fn act(&mut self, keys: &Keys, key: Key, mods: Mods) -> Outcome {
        let list = &mut self.list;
        if self.editing && !mods.contains(Mods::CTRL) {
            match key {
                Key::Esc => {
                    list.clear_search();
                    self.editing = false;
                }
                Key::Enter => self.editing = false,
                Key::Backspace => list.pop_search(),
                Key::Char(c) => list.push_search(c),
                _ => {}
            }
            return Outcome::Stay;
        }
        let page = isize::try_from(self.height).unwrap_or(isize::MAX);
        let plain = !mods.contains(Mods::CTRL) && !mods.contains(Mods::ALT);
        // The tree's key for files with comments filters the threads here,
        // and its capital steps back.
        let filter_back = |c: char| {
            c.is_ascii_uppercase()
                && keys.action(true, Key::Char(c.to_ascii_lowercase()), Mods::NONE)
                    == Some(Act::CommentedFiles)
        };
        match (key, keys.action(true, key, mods)) {
            (Key::Esc, _) if !list.search().is_empty() => list.clear_search(),
            (Key::Esc, _) => return Outcome::Diff,
            (Key::Enter, _) | (_, Some(Act::Open)) => return Outcome::Open,
            (_, Some(Act::CommentedFiles)) => list.step_filter(false),
            (Key::Char(c), _) if plain && filter_back(c) => list.step_filter(true),
            (Key::Char(OTHER_BRANCHES), _) if plain => list.toggle_others(),
            (_, Some(Act::FilterTree)) => self.editing = true,
            (_, Some(Act::ScrollDown)) => self.selected = list.step(self.selected, 1),
            (_, Some(Act::ScrollUp)) => self.selected = list.step(self.selected, -1),
            (_, Some(Act::PageDown | Act::HalfPageDown)) => {
                self.selected = list.step(self.selected, page);
            }
            (_, Some(Act::PageUp | Act::HalfPageUp)) => {
                self.selected = list.step(self.selected, -page);
            }
            (_, Some(Act::Top)) => self.selected = list.step(self.selected, isize::MIN),
            (_, Some(Act::Bottom)) => self.selected = list.step(self.selected, isize::MAX),
            // The tree's, which isn't shown.
            (_, Some(Act::Collapse | Act::MarkViewed)) => {}
            _ => return Outcome::Pass,
        }
        Outcome::Stay
    }

    /// The title's parts: every thread and the open ones, or with a filter
    /// or a search, how many of them it keeps and what it is.
    fn title(&self, listing: &Listing) -> Vec<(String, Role)> {
        let [all, open] = listing.counts;
        let mut parts = vec![(" Comments".to_owned(), Role::Plain)];
        if self.list.unfiltered() {
            let s = if all == 1 { "" } else { "s" };
            parts.push((format!(" · {all} thread{s}"), Role::Dim));
            parts.push((format!(" · {open} open"), Role::Dim));
            return parts;
        }
        let kept = self.list.kept();
        parts.push((format!(" · {kept}/{all}"), Role::Dim));
        let filter = self.list.filter_name();
        if !filter.is_empty() {
            parts.push((format!(" · {filter}"), Role::Hint));
        }
        let search = self.list.search();
        if !search.is_empty() {
            parts.push((format!(" · /{search}"), Role::Hint));
        }
        parts
    }

    /// Draws the panel into columns `0..width` of rows `top..top + height`,
    /// scrolled to show the selected thread and, for a file's first, its
    /// header; see [`tree::frame`] for `joined`.
    pub(super) fn draw(
        &mut self,
        grid: &mut Grid,
        (top, height, width): (usize, usize, usize),
        (comments, listing): (&Comments, bool),
        (focused, joined): (bool, bool),
        (theme, keys): (&Theme, &Keys),
    ) {
        if height < 3 || width < 5 {
            return;
        }
        self.sync(comments, listing);
        self.height = height - 2;
        let rows = self.list.rows();
        let selected = self.thread().is_some().then_some(self.selected);
        if let Some(at) = selected {
            let first = match at.checked_sub(1).map(|i| &rows[i]) {
                Some(Row::File { .. }) => at - 1,
                _ => at,
            };
            if first < self.scroll {
                self.scroll = first;
            } else if at >= self.scroll + self.height {
                self.scroll = at + 1 - self.height;
            }
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(self.height));
        let Some(list) = self.list.listing() else {
            return;
        };
        let typing = self
            .editing
            .then_some((self.list.search(), "type to search · esc clear "));
        tree::frame(
            grid,
            (top, height, width),
            &self.title(list),
            typing,
            (focused, joined),
            theme,
        );
        let (x, right) = (2, width - 2);
        let mut c = Canvas::default();
        for row in rows.iter().skip(self.scroll).take(self.height) {
            list_row(&mut c, (list, keys, self.opens), row, right - x);
        }
        for (i, cells) in c.rows().enumerate() {
            let y = top + 1 + i;
            paint(grid, x, y, right, cells, theme, Attributes::NONE);
            if selected == Some(self.scroll + i) {
                grid.map_styles(1, y, width - 1, |s| theme.selected(s, focused));
            }
        }
    }
}

/// The title of the conversation's group, or the other branches' with the
/// key that moves a thread here when one does.
fn group_title(conversation: bool, keys: &Keys) -> (&'static str, String) {
    if conversation {
        return ("Conversation", String::new());
    }
    let key = keys.chords(false, Act::MoveThread).next();
    let hint = key.map_or_else(String::new, |k| format!(" · {} moves one here", k.name()));
    ("Other branches", hint)
}

/// Writes `row` of the panel's list, `width` columns wide. A file header's
/// folders are cut from the left to leave its name whole; one the diff
/// doesn't list says whether `enter` opens it (with `opens`, unless it's
/// gone from the working tree). A thread's replies are marked (`↩2`) so
/// they don't read as the header's count of threads.
fn list_row(c: &mut Canvas, (list, keys, opens): (&Listing, &Keys, bool), row: &Row, width: usize) {
    match row {
        Row::Note(words) => {
            let text = model::say(words, |hint| super::app::hint_words(keys, hint));
            cut(c, &[(Role::Dim, text.as_str())], width);
        }
        Row::File { group, threads }
            if list.files[*group].conversation || list.files[*group].branches =>
        {
            let (title, hint) = group_title(list.files[*group].conversation, keys);
            let title = [(AUTHOR, title), (DIM, hint.as_str())];
            spread(c, &title, &[(Role::Dim, &threads.to_string())], width);
        }
        Row::File { group, threads } => {
            let group = &list.files[*group];
            let path = escape(group.path.as_bytes());
            let (dir, name) = path.split_at(path.rfind('/').map_or(0, |i| i + 1));
            let absent = match (group.absent, opens) {
                (None, _) => "",
                (Some(_), false) => " · not in this diff",
                (Some(Absent::Unlisted), true) => " · enter opens the file",
                (Some(Absent::Gone), true) => " · file not on disk",
            };
            let count = threads.to_string();
            let room = width.saturating_sub(count.width() + 2);
            let dir = tail(dir, room.saturating_sub(name.width() + absent.width()));
            spread(
                c,
                &[
                    (Role::Dim.into(), &dir),
                    (AUTHOR, name),
                    (Role::Dim.into(), absent),
                ],
                &[(Role::Dim, &count)],
                width,
            );
        }
        Row::Thread { item, author } => {
            let item = &list.items[*item];
            let thread = item.thread();
            let Some(first) = thread.comments.first() else {
                c.end_line();
                return;
            };
            let ink = |role: Role| -> Ink {
                if thread.resolved {
                    Role::Dim.into()
                } else {
                    role.into()
                }
            };
            let line = item
                .line
                .filter(|_| width >= LINE_COLUMNS)
                .map(|n| format!(":{n} "));
            let moved = Ink {
                attributes: Attributes::DIM,
                ..Role::Warn.into()
            };
            let (kind, label) = match &thread.at {
                At::Conversation { kind, label } => (shown_kind(kind.as_deref()), label.as_deref()),
                _ => (None, None),
            };
            let label = label.map(|l| escape(l.as_bytes()));
            let branch = thread.branch.as_deref().map(|b| escape(b.as_bytes()));
            let words = [
                (thread.at == At::Lost && list.files[item.group].absent.is_none())
                    .then_some((moved, "moved")),
                branch.as_deref().map(|b| (ink(Role::Hint), b)),
                thread.local.then_some((ink(Role::Hint), "branch draft")),
                row_word(first.standing).map(|word| (ink(Role::Dim), word)),
                kind.map(|k| (ink(Role::Dim), k)),
                label.as_deref().map(|l| (ink(Role::Hint), l)),
            ];
            let name = author.then(|| escape(who(&first.author).as_bytes()));
            let mut spans = vec![
                (ink(Role::Plain), "  "),
                (ink(look_role(thread.look)), glyph(thread.look)),
                (ink(Role::Plain), " "),
            ];
            spans.extend(line.as_deref().map(|l| (Role::LineNumber.into(), l)));
            for (word_ink, word) in words.into_iter().flatten() {
                spans.extend([(word_ink, word), (ink(Role::Plain), " ")]);
            }
            if let Some(name) = &name {
                let bold = Ink {
                    attributes: Attributes::BOLD,
                    ..ink(Role::Plain)
                };
                spans.extend([(bold, name.as_str()), (ink(Role::Plain), " ")]);
            }
            spans.push((ink(Role::Plain), &first.summary));
            let replies = match thread.comments.len() - 1 {
                0 => String::new(),
                _ if width < REPLIES_COLUMNS => String::new(),
                n => format!("↩{n}"),
            };
            spread(c, &spans, &[(Role::Dim, &replies)], width);
        }
    }
    c.end_line();
}

/// The end of `text` that fits `width` columns, after `…` when that's not
/// all of it.
fn tail(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = 1;
    let mut start = text.len();
    let clusters: Vec<_> = text.clusters().collect();
    for &(i, _, w) in clusters.iter().rev() {
        used += w;
        if used > width {
            break;
        }
        start = i;
    }
    format!("…{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Offsets and fractions count; past a month an age is the date, and
    /// what isn't a time shows as given.
    #[test]
    fn ages_read_rfc_3339_times() {
        let now = epoch("2026-11-17T12:00:00Z").expect("a time");
        assert_eq!(epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch("2026-11-17T13:00:00+01:00"), Some(now));
        assert_eq!(age("2026-11-17T11:59:30.123Z", now), "just now");
        assert_eq!(age("2026-11-17T11:55:00Z", now), "5m ago");
        assert_eq!(age("2026-11-17T10:00:00Z", now), "2h ago");
        assert_eq!(age("2026-11-14T12:00:00Z", now), "3d ago");
        assert_eq!(age("2026-09-01T12:00:00Z", now), "2026-09-01");
        assert_eq!(age("yesterday", now), "yesterday");
    }

    /// A new thread's box heading says what it writes, its lines and what
    /// `enter` does; narrower, the keys go first, then the lines, then the
    /// agent asked, then what it writes.
    #[test]
    fn a_new_threads_heading_says_its_lines_and_what_enter_does() {
        let top = |at: Writes, enter: Enter, width: usize| {
            let mut writing = Writing {
                at,
                text: TextBox::new(""),
                enter,
            };
            let mut c = Canvas::default();
            new_rows(&mut writing, width, &mut c);
            (c.rows().next())
                .map(|row| row.map(|(_, text)| text).collect::<String>())
                .unwrap_or_default()
        };
        let lines = |lines| Writes::New {
            side: Side::New,
            lines,
        };
        let claude = || Enter::Asks(Some("claude".to_owned()));
        let fixes = |session: Option<&str>| Enter::Fixes(session.map(str::to_owned));
        // Parts go whole: only a box too narrow for `▾ you` cuts.
        for width in 9..120 {
            for enter in [claude(), Enter::Asks(None), Enter::Saves, fixes(None)] {
                let heading = top(lines(318..331), enter, width);
                assert!(!heading.contains('…'), "{width}: {heading}");
            }
        }
        let shown = [
            top(lines(318..331), claude(), 100),
            top(lines(318..331), Enter::Asks(None), 100),
            top(lines(318..331), Enter::Saves, 100),
            top(lines(41..42), Enter::Saves, 100),
            top(Writes::File, Enter::Saves, 100),
            top(lines(41..42), fixes(None), 100),
            top(lines(41..42), fixes(Some("claude · edited 3h ago")), 100),
            top(lines(318..331), claude(), 80),
            top(lines(318..331), claude(), 50),
            top(lines(318..331), claude(), 30),
            top(lines(318..331), fixes(Some("claude")), 24),
        ];
        insta::assert_snapshot!(shown.join("\n"), @"
        ┏ ▾ you · question for claude · lines 319–331 · enter asks · alt+enter new line · ctrl+g editor ━━━┓
        ┏ ▾ you · question for the agent · lines 319–331 · enter asks · alt+enter new line · ctrl+g editor ┓
        ┏ ▾ you · new comment · lines 319–331 · enter saves · alt+enter new line · ctrl+g editor ━━━━━━━━━━┓
        ┏ ▾ you · new comment · line 42 · enter saves · alt+enter new line · ctrl+g editor ━━━━━━━━━━━━━━━━┓
        ┏ ▾ you · new comment · whole file · enter saves · alt+enter new line · ctrl+g editor ━━━━━━━━━━━━━┓
        ┏ ▾ you · fix with agent · line 42 · enter sends it · alt+enter new line · ctrl+g editor ━━━━━━━━━━┓
        ┏ ▾ you · for claude · edited 3h ago · line 42 · enter sends it · alt+enter new line ━━━━━━━━━━━━━━┓
        ┏ ▾ you · question for claude · lines 319–331 · enter asks ━━━━━━━━━━━━━━━━━━━━┓
        ┏ ▾ you · question for claude · lines 319–331 ━━━┓
        ┏ ▾ you · question ━━━━━━━━━━┓
        ┏ ▾ you · for claude ━━┓
        ");
    }
}

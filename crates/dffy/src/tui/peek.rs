//! Go to definition on screen: the list of names on a line to pick one
//! from, and the peek, a block hanging under the line that asked, with a
//! definition's code beside the list of the name's definitions. What's
//! looked up, and what the keys do, is `app::peek`'s; this draws and
//! hit-tests.
//!
//! The peek is always [`HEIGHT`] rows, whatever it shows, so the index's
//! progress, the code arriving and moving through the list never change
//! the rows around it.

use std::sync::Arc;

use dffy_core::highlight::DefinitionKind;
use dffy_core::navigate::{self, MAX_REFERENCE_FILES};
use dffy_core::view::{Attributes, Run};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::help;
use super::worker::{Code, Looked};
use crate::render::{Canvas, thousands};
use crate::review::model::snippet::pieces;
use crate::term::{Grid, Key, Mods};
use crate::theme::{Ink, Role, Style, Theme};

/// Rows the peek takes: its top border with the header, the body, and its
/// bottom border with the keys.
pub(crate) const HEIGHT: usize = 14;
const BODY: usize = HEIGHT - 2;
/// Boxes at least this wide put the list beside the code; narrower ones
/// put it above.
const WIDE: usize = 90;
/// The list's rows above the code in a narrow box.
const NARROW_LIST: usize = 4;
/// Lines of code shown above the definition's first.
const ABOVE: usize = 2;
/// What the list of names on a line does with a key or a tap.
pub(crate) enum Picked {
    Stay,
    Close,
    Name(String),
}

/// The names on a line, to pick the one to look up: typing narrows them
/// fuzzily, arrows move, `enter` picks, `esc` closes.
pub(crate) struct Names {
    all: Vec<String>,
    query: String,
    shown: Vec<usize>,
    cursor: usize,
    /// The screen cell it opens at, moved in as far as the screen needs.
    at: (usize, usize),
}

/// Names the list shows at once.
const NAMES_SHOWN: usize = 10;
const NAMES_TITLE: &str = " go to definition ";

impl Names {
    /// `names` has at least one.
    pub(crate) fn new(names: Vec<String>, at: (usize, usize)) -> Self {
        let shown = (0..names.len()).collect();
        Self {
            all: names,
            query: String::new(),
            shown,
            cursor: 0,
            at,
        }
    }

    fn narrow(&mut self) {
        let mut scored: Vec<(i64, usize)> = (self.all.iter().enumerate())
            .filter_map(|(i, n)| Some((crate::fuzzy::score(&self.query, n)?.0, i)))
            .collect();
        if !self.query.is_empty() {
            scored.sort_by_key(|&(score, i)| (std::cmp::Reverse(score), i));
        }
        self.shown = scored.into_iter().map(|(_, i)| i).collect();
        self.cursor = 0;
    }

    pub(crate) fn key(&mut self, key: Key, mods: Mods) -> Picked {
        let ctrl = mods.contains(Mods::CTRL);
        let len = self.shown.len();
        match key {
            Key::Esc => return Picked::Close,
            Key::Char('c') if ctrl => return Picked::Close,
            Key::Enter => {
                return match self.shown.get(self.cursor) {
                    Some(&i) => Picked::Name(self.all[i].clone()),
                    None => Picked::Stay,
                };
            }
            Key::Down | Key::Tab if len > 0 => self.cursor = (self.cursor + 1) % len,
            Key::Char('n') if ctrl && len > 0 => self.cursor = (self.cursor + 1) % len,
            Key::Up | Key::BackTab if len > 0 => self.cursor = (self.cursor + len - 1) % len,
            Key::Char('p') if ctrl && len > 0 => self.cursor = (self.cursor + len - 1) % len,
            Key::Backspace => {
                self.query.pop();
                self.narrow();
            }
            Key::Char(c) if !ctrl && (c.is_alphanumeric() || c == '_') => {
                self.query.push(c);
                self.narrow();
            }
            _ => {}
        }
        Picked::Stay
    }

    /// A tap at cell `(x, y)` of a `size` screen: a name is picked; on the
    /// border or the query nothing happens; outside, the list closes.
    pub(crate) fn tap(&mut self, (x, y): (usize, usize), size: (usize, usize)) -> Picked {
        let (x0, y0, width, height) = self.rect(size);
        if !(x0..x0 + width).contains(&x) || !(y0..y0 + height).contains(&y) {
            return Picked::Close;
        }
        let first = self.first_shown();
        match (y - y0).checked_sub(2) {
            Some(row) if y + 1 < y0 + height => match self.shown.get(first + row) {
                Some(&i) => Picked::Name(self.all[i].clone()),
                None => Picked::Stay,
            },
            _ => Picked::Stay,
        }
    }

    fn first_shown(&self) -> usize {
        self.cursor.saturating_sub(NAMES_SHOWN - 1)
    }

    fn rect(&self, (gw, gh): (usize, usize)) -> (usize, usize, usize, usize) {
        let widest = (self.all.iter().map(|n| n.width()))
            .chain([NAMES_TITLE.width(), self.query.width() + 2])
            .max()
            .unwrap_or(0);
        let width = (widest + 4).min(gw);
        let height = (self.all.len().min(NAMES_SHOWN) + 3).min(gh);
        (
            self.at.0.min(gw.saturating_sub(width)),
            self.at.1.min(gh.saturating_sub(height)),
            width,
            height,
        )
    }

    pub(crate) fn draw(&self, grid: &mut Grid, theme: &Theme) {
        let (x0, y0, width, height) = self.rect((grid.width(), grid.height()));
        if width < 6 || height < 4 {
            return;
        }
        help::border(grid, (x0, y0, width, height), theme);
        let end = x0 + width - 2;
        grid.put_str(x0 + 2, y0, end, NAMES_TITLE, theme.style(Role::Dim));
        let x = x0 + 2;
        let query = format!("› {}▏", self.query);
        grid.put_str(x, y0 + 1, end, &query, theme.style(Role::Hint));
        let first = self.first_shown();
        for (row, &i) in self.shown.iter().skip(first).take(height - 3).enumerate() {
            let y = y0 + 2 + row;
            let mut style = Style::PLAIN;
            if first + row == self.cursor {
                style = theme.selected(style, true);
                grid.fill(x0 + 1, y, x0 + width - 1, style);
                style.attributes |= Attributes::BOLD;
            }
            grid.put_str(x, y, end, &self.all[i], style);
        }
        if self.shown.is_empty() {
            grid.put_str(x, y0 + 2, end, "no name matches", theme.style(Role::Dim));
        }
    }
}

/// What the peek has found so far.
pub(crate) enum Found {
    /// Waiting for the lookup, as these words say.
    Waiting(String),
    /// The definitions found, or the text search's lines.
    Listed(Looked),
    /// Nothing to list, as these words say.
    Nothing(String),
}

/// The lines using the name, as far as the search has found them.
#[derive(Debug, Clone, Default)]
pub(crate) struct Refs {
    /// Those of the first [`navigate::MAX_REFERENCE_FILES`] files with one.
    pub hits: Vec<navigate::Candidate>,
    /// Each hit's line.
    pub lines: Vec<Option<String>>,
    /// A file's uses past [`navigate::MAX_REFERENCE_LINES`]: the index of
    /// its last hit listed and how many more, in order.
    pub more: Vec<(usize, usize)>,
    /// Files with a use found so far.
    pub files: usize,
    pub searched: navigate::Progress,
    pub done: bool,
    /// Some files were searched before the index had them: their
    /// definitions may be listed as uses.
    pub partial: bool,
    /// Why the search stopped.
    pub failed: Option<String>,
    /// It stopped at its first [`navigate::MAX_REFERENCE_FILES`] files,
    /// to go on only when asked.
    pub capped: bool,
}

/// A peek at a name in one version of the diff: its definitions, and below
/// them the lines using it, each group filling in as it's found.
pub(crate) struct Peek {
    pub name: String,
    /// The version's name, as the viewer says it.
    pub version: String,
    found: Found,
    /// The uses, once their search started.
    refs: Option<Refs>,
    /// The list's rows, made when what it shows changes.
    items: Vec<Item>,
    /// Said after the header: how far the index is built when it's not.
    pub note: Option<String>,
    /// What the uses' group says of the filters on, and of the key going
    /// past the first files.
    pub uses_words: String,
    pub more_key: String,
    /// The candidate shown, of the definitions then the uses, and where it
    /// was in the other group.
    cursor: usize,
    parked: usize,
    /// The cursor goes to the first use once there is one, untouched: the
    /// name was asked from its own definition.
    on_uses: bool,
    /// The cursor was moved since the peek opened.
    moved: bool,
    /// The code of the file of the candidate at `cursor`, or why there's
    /// none; `None` while it's read.
    pub code: Option<Result<Arc<Code>, String>>,
    /// It has the keys.
    pub focused: bool,
    /// What the keys do, as the bottom border says it.
    pub hints: String,
}

/// A row of the list: a group's heading, a file's path, then its
/// candidates' lines. Candidates are numbered through both groups, the
/// definitions first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Group(Group),
    /// The file of the candidate at this index, and those after it.
    File(usize),
    Candidate(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Definitions,
    Uses,
}

/// Pushes the list's rows for `listed`'s candidates, numbered from
/// `first`.
fn push_items(listed: &[navigate::Candidate], first: usize, out: &mut Vec<Item>) {
    let mut last = None;
    for (i, c) in listed.iter().enumerate() {
        if last != Some(c.path.as_slice()) {
            out.push(Item::File(first + i));
            last = Some(c.path.as_slice());
        }
        out.push(Item::Candidate(first + i));
    }
}

/// A row being written, cut at `room` columns, tabs spread to stops
/// `tab` apart from column `origin`.
struct Cells<'c> {
    c: &'c mut Canvas,
    used: usize,
    room: usize,
    tab: usize,
    origin: usize,
    /// Something didn't fit: nothing more is written but the fill.
    cut: bool,
}

impl<'c> Cells<'c> {
    fn new(c: &'c mut Canvas, room: usize, tab: usize) -> Self {
        Self {
            c,
            used: 0,
            room,
            tab: tab.max(1),
            origin: 0,
            cut: false,
        }
    }

    fn put(&mut self, ink: impl Into<Ink>, text: &str) {
        let mut out = String::new();
        for ch in text.chars() {
            if self.cut || self.used >= self.room {
                break;
            }
            if ch == '\t' {
                let at = self.used.saturating_sub(self.origin);
                let n = (self.tab - at % self.tab).min(self.room - self.used);
                out.extend(std::iter::repeat_n(' ', n));
                self.used += n;
                continue;
            }
            if ch.is_control() {
                continue;
            }
            let w = ch.width().unwrap_or(0);
            if self.used + w > self.room {
                self.cut = true;
                break;
            }
            out.push(ch);
            self.used += w;
        }
        self.c.push_ink(ink.into(), &out);
    }

    /// The rest of the row, as `ch` repeated.
    fn fill(&mut self, ink: impl Into<Ink>, ch: char) {
        let n = self.room.saturating_sub(self.used);
        let text: String = std::iter::repeat_n(ch, n).collect();
        self.c.push_ink(ink.into(), &text);
        self.used = self.room;
    }
}

impl Peek {
    /// A peek at `name` in `version`, waiting as `waiting` says.
    pub(crate) fn new(name: String, version: String, waiting: String, hints: String) -> Self {
        Self {
            name,
            version,
            found: Found::Waiting(waiting),
            refs: None,
            items: Vec::new(),
            note: None,
            uses_words: String::new(),
            more_key: String::new(),
            cursor: 0,
            parked: 0,
            on_uses: false,
            moved: false,
            code: None,
            focused: true,
            hints,
        }
    }

    pub(crate) fn found(&self) -> &Found {
        &self.found
    }

    /// The definitions listed, or the text search's lines.
    fn definitions(&self) -> &[navigate::Candidate] {
        match &self.found {
            Found::Listed(looked) => &looked.candidates,
            Found::Waiting(_) | Found::Nothing(_) => &[],
        }
    }

    fn uses(&self) -> &[navigate::Candidate] {
        self.refs.as_ref().map_or(&[], |r| &r.hits)
    }

    /// Shows `found`, the cursor on definition `cursor` unless it was moved
    /// to a use, where it stays.
    pub(crate) fn set_found(&mut self, found: Found, cursor: usize) {
        let before = self.definitions().len();
        let on_use = self.moved && self.cursor >= before && !self.uses().is_empty();
        self.found = found;
        let after = self.definitions().len();
        if on_use {
            self.cursor = self.cursor - before + after;
        } else {
            self.cursor = cursor.min(after.saturating_sub(1));
            self.code = None;
        }
        self.items = self.make_items();
        self.settle_on_uses();
    }

    /// The cursor goes to the first use once there is one, as long as it
    /// wasn't moved: the name was asked from its own definition.
    pub(crate) fn open_on_uses(&mut self) {
        self.on_uses = true;
        self.settle_on_uses();
    }

    fn settle_on_uses(&mut self) {
        if self.on_uses && !self.moved && !self.uses().is_empty() {
            self.parked = self.cursor;
            self.cursor = self.definitions().len();
            self.on_uses = false;
            self.code = None;
        }
    }

    /// The uses, once their search started.
    pub(crate) fn references(&self) -> Option<&Refs> {
        self.refs.as_ref()
    }

    /// The cursor is on a use.
    pub(crate) fn on_a_use(&self) -> bool {
        self.cursor >= self.definitions().len() && !self.uses().is_empty()
    }

    /// Changes the uses with `f`.
    pub(crate) fn edit_references(&mut self, f: impl FnOnce(&mut Refs)) {
        let defs = self.definitions().len();
        f(self.refs.get_or_insert_with(Refs::default));
        let len = defs + self.uses().len();
        if self.cursor >= len {
            self.cursor = len.saturating_sub(1);
            self.code = None;
        }
        self.items = self.make_items();
    }

    /// Adds a step's uses after those listed, as `f` puts them in; the
    /// cursor stays on its candidate.
    pub(crate) fn add_references(&mut self, f: impl FnOnce(&mut Refs)) {
        f(self.refs.get_or_insert_with(Refs::default));
        self.items = self.make_items();
        self.settle_on_uses();
    }

    /// Jumps between the definitions and the uses, each group keeping
    /// where it was; whether it moved.
    pub(crate) fn jump_group(&mut self) -> bool {
        let (defs, uses) = (self.definitions().len(), self.uses().len());
        let to = if self.cursor >= defs {
            if defs == 0 {
                return false;
            }
            if self.parked < defs { self.parked } else { 0 }
        } else {
            if uses == 0 {
                return false;
            }
            if (defs..defs + uses).contains(&self.parked) {
                self.parked
            } else {
                defs
            }
        };
        self.parked = self.cursor;
        self.cursor = to;
        self.moved = true;
        self.code = None;
        true
    }

    /// Candidate `i`'s line, as read.
    fn line(&self, i: usize) -> Option<&String> {
        let defs = self.definitions().len();
        let lines = match (&self.found, &self.refs) {
            (Found::Listed(looked), _) if i < defs => looked.lines.get(i),
            (_, Some(refs)) if i >= defs => refs.lines.get(i - defs),
            _ => None,
        };
        lines.and_then(Option::as_ref)
    }

    /// Puts the cursor on candidate `i`; whether it moved.
    pub(crate) fn set_cursor(&mut self, i: usize) -> bool {
        let moved = i != self.cursor && self.get(i).is_some();
        if moved {
            self.cursor = i;
            self.moved = true;
        }
        moved
    }

    /// Candidate `i`, of the definitions then the uses.
    fn get(&self, i: usize) -> Option<&navigate::Candidate> {
        let defs = self.definitions();
        match i.checked_sub(defs.len()) {
            None => defs.get(i),
            Some(u) => self.uses().get(u),
        }
    }

    /// The candidate shown, if any.
    pub(crate) fn current(&self) -> Option<&navigate::Candidate> {
        self.get(self.cursor)
    }

    /// Moves the cursor by `by` candidates, through both groups; whether
    /// it moved.
    pub(crate) fn step(&mut self, by: isize) -> bool {
        let len = self.definitions().len() + self.uses().len();
        let to = self
            .cursor
            .saturating_add_signed(by)
            .min(len.saturating_sub(1));
        let moved = to != self.cursor;
        self.cursor = to;
        self.moved |= moved;
        moved
    }

    fn make_items(&self) -> Vec<Item> {
        let mut out = Vec::new();
        let defs = self.definitions();
        // Without definitions, the group's row says why, over the uses.
        if !defs.is_empty() || self.refs.is_some() {
            out.push(Item::Group(Group::Definitions));
            push_items(defs, 0, &mut out);
        }
        if self.refs.is_some() {
            out.push(Item::Group(Group::Uses));
            push_items(self.uses(), defs.len(), &mut out);
        }
        out
    }

    /// The items shown in `rows` rows, from the first: the cursor's
    /// candidate shows, with its file's row when there's room.
    fn window(&self, rows: usize) -> &[Item] {
        let (cursor, defs) = (self.cursor, self.definitions().len());
        // Items go by candidate, each file's and group's row before its
        // first.
        let at = self.items.partition_point(|item| match *item {
            Item::Group(Group::Definitions) => true,
            Item::Group(Group::Uses) => defs <= cursor,
            Item::File(i) => i <= cursor,
            Item::Candidate(i) => i < cursor,
        });
        let at = if self.items.get(at) == Some(&Item::Candidate(cursor)) {
            at
        } else {
            0
        };
        let start = if at < rows {
            0
        } else {
            at.saturating_sub(1).min(at + 1 - rows)
        };
        &self.items[start..]
    }

    /// How wide the box's list is beside the code, when it's beside it.
    fn columns(width: usize) -> Option<(usize, usize)> {
        (width >= WIDE).then(|| {
            let list = (width * 2 / 5).min(56);
            (width - 7 - list, list)
        })
    }

    /// The candidate on row `line` of the peek, at column `column` from
    /// its left, when the box is `width` wide.
    pub(crate) fn candidate_at(&self, width: usize, line: usize, column: usize) -> Option<usize> {
        let row = line.checked_sub(1)?;
        let rows = match Self::columns(width) {
            Some((code, _)) if column >= 2 + code + 3 => BODY,
            Some(_) => return None,
            None => NARROW_LIST,
        };
        if row >= rows {
            return None;
        }
        match self.window(rows).get(row)? {
            Item::Candidate(i) | Item::File(i) => Some(*i),
            Item::Group(_) => None,
        }
    }

    /// Writes the peek, [`HEIGHT`] rows `width` columns wide, into `c`.
    pub(crate) fn rows(&self, width: usize, tab: usize, c: &mut Canvas) {
        let border: Ink = if self.focused {
            Role::Hint.into()
        } else {
            Role::Dim.into()
        };
        let tab = tab.max(1);
        let inner = width.saturating_sub(4);
        // The top border, with the header.
        c.push_ink(border, "┌ ");
        let mut cells = Cells::new(c, width.saturating_sub(3), tab);
        self.header(&mut cells);
        cells.put(border, " ");
        cells.fill(border, '─');
        c.push_ink(border, "┐");
        c.end_line();
        if let Some((code, list)) = Self::columns(width) {
            let items = self.window(BODY);
            for row in 0..BODY {
                c.push_ink(border, "│ ");
                self.code_row(row, code, tab, c);
                c.push_ink(border, " │ ");
                self.list_row(items.get(row).copied(), list, tab, c);
                c.push_ink(border, " │");
                c.end_line();
            }
        } else {
            let items = self.window(NARROW_LIST);
            for row in 0..NARROW_LIST {
                c.push_ink(border, "│ ");
                self.list_row(items.get(row).copied(), inner, tab, c);
                c.push_ink(border, " │");
                c.end_line();
            }
            c.push_ink(border, "├");
            let rule: String = std::iter::repeat_n('─', width.saturating_sub(2)).collect();
            c.push_ink(border, &rule);
            c.push_ink(border, "┤");
            c.end_line();
            for row in 0..BODY - NARROW_LIST - 1 {
                c.push_ink(border, "│ ");
                self.code_row(row, inner, tab, c);
                c.push_ink(border, " │");
                c.end_line();
            }
        }
        // The bottom border, with the keys.
        c.push_ink(border, "└ ");
        let mut cells = Cells::new(c, width.saturating_sub(3), tab);
        cells.put(Role::Hint, &self.hints);
        cells.put(border, " ");
        cells.fill(border, '─');
        c.push_ink(border, "┘");
        c.end_line();
    }

    /// `name · N definitions · path:line · version`, and what's known of
    /// the one shown.
    fn header(&self, cells: &mut Cells) {
        let name = Ink {
            attributes: Attributes::BOLD,
            ..Role::Plain.into()
        };
        cells.put(name, &self.name);
        let dot = |cells: &mut Cells| cells.put(Role::Dim, " · ");
        match &self.found {
            Found::Waiting(_) => {
                dot(cells);
                cells.put(Role::Dim, "looking…");
            }
            Found::Nothing(words) => {
                dot(cells);
                cells.put(Role::Dim, words);
            }
            Found::Listed(looked) => {
                dot(cells);
                let what = match (looked.text, looked.total) {
                    (true, 1) => "1 text match".to_owned(),
                    (true, n) if !looked.complete => format!("{}+ text matches", thousands(n)),
                    (true, n) => format!("{} text matches", thousands(n)),
                    (false, 1) => "1 definition".to_owned(),
                    (false, n) => format!("{} definitions", thousands(n)),
                };
                cells.put(Role::Plain, &what);
                if looked.total > looked.candidates.len() {
                    cells.put(
                        Role::Dim,
                        &format!(" (first {})", thousands(looked.candidates.len())),
                    );
                }
            }
        }
        if let Some(c) = self.current() {
            dot(cells);
            let path = String::from_utf8_lossy(&c.path);
            cells.put(Role::FileHeader, &format!("{path}:{}", c.start + 1));
        }
        dot(cells);
        cells.put(Role::Plain, &self.version);
        if let Some(c) = self.current() {
            if c.in_hunk == Some(true) {
                dot(cells);
                cells.put(Role::Warn, "changed in this diff");
            } else if c.changed {
                dot(cells);
                cells.put(Role::Dim, "its file changed in this diff");
            }
        }
        if let Some(note) = &self.note {
            dot(cells);
            cells.put(Role::Dim, note);
        }
    }

    /// What the code's place says while nothing is listed.
    fn empty(&self) -> String {
        match &self.found {
            Found::Waiting(words) | Found::Nothing(words) => words.clone(),
            Found::Listed(_) => String::new(),
        }
    }

    /// The uses' group's heading: how far their search is, and the filters
    /// on.
    fn uses_heading(&self) -> String {
        let Some(refs) = &self.refs else {
            return String::new();
        };
        let files = |n: usize| {
            let s = if n == 1 { "" } else { "s" };
            format!("{} file{s}", thousands(n))
        };
        let mut words = match (&refs.failed, refs.done, refs.capped) {
            (Some(why), ..) => format!("Uses · the search stopped: {why}"),
            (None, true, _) if refs.files == 0 => "Uses · none".to_owned(),
            (None, true, _) => format!("Uses · {}", files(refs.files)),
            (None, false, true) => format!(
                "Uses · first {} files · {}",
                MAX_REFERENCE_FILES, self.more_key
            ),
            (None, false, false) => format!(
                "Uses · {} so far · searching {} of {}",
                files(refs.files),
                thousands(refs.searched.done),
                thousands(refs.searched.total)
            ),
        };
        if !self.uses_words.is_empty() {
            words.push_str(" · ");
            words.push_str(&self.uses_words);
        }
        if refs.partial {
            words.push_str(" (index partial: definitions may show)");
        }
        words
    }

    /// Row `row` of the code, `room` columns wide.
    fn code_row(&self, row: usize, room: usize, tab: usize, c: &mut Canvas) {
        let mut cells = Cells::new(c, room, tab);
        let say = |cells: &mut Cells, text: &str| {
            if row == 0 {
                cells.put(Role::Dim, text);
            }
        };
        match (self.current(), &self.code) {
            (None, _) => say(&mut cells, &self.empty()),
            (Some(_), None) => say(&mut cells, "reading…"),
            (Some(_), Some(Err(why))) => say(&mut cells, &format!("can't show it: {why}")),
            (Some(_), Some(Ok(code))) => {
                let first = self.current().map_or(0, |c| c.start.saturating_sub(ABOVE));
                let line = first + row;
                if let Some(text) = code.lines.get(line) {
                    let digits = (first + BODY).min(code.lines.len()).to_string().len();
                    let defining = self
                        .current()
                        .is_some_and(|c| (c.start..=c.end).contains(&line));
                    let number = if defining {
                        Role::Hint
                    } else {
                        Role::LineNumber
                    };
                    cells.put(number, &format!("{:>digits$} ", line + 1));
                    cells.origin = cells.used;
                    let runs: Option<&[Run]> = code.spans.as_ref().map(|s| s.line(line));
                    for (syntax, piece) in pieces(text, runs) {
                        let ink = Ink {
                            syntax,
                            ..Role::Plain.into()
                        };
                        cells.put(ink, &piece);
                    }
                }
            }
        }
        cells.fill(Role::Plain, ' ');
    }

    /// A row of the list, `room` columns wide.
    fn list_row(&self, item: Option<Item>, room: usize, tab: usize, c: &mut Canvas) {
        let mut cells = Cells::new(c, room, tab);
        let defs = self.definitions().len();
        let text_matches = matches!(&self.found, Found::Listed(l) if l.text);
        match item {
            Some(Item::Group(Group::Definitions)) => {
                let what = match &self.found {
                    Found::Waiting(_) => "Definitions · looking…".to_owned(),
                    Found::Nothing(_) => "Definitions · none".to_owned(),
                    Found::Listed(_) if text_matches => {
                        format!("Text matches ({})", thousands(defs))
                    }
                    Found::Listed(_) => format!("Definitions ({})", thousands(defs)),
                };
                cells.put(Role::Dim, &what);
                cells.fill(Role::Plain, ' ');
            }
            Some(Item::Group(Group::Uses)) => {
                cells.put(Role::Dim, &self.uses_heading());
                cells.fill(Role::Plain, ' ');
            }
            Some(Item::File(i)) => {
                let Some(c) = self.get(i) else {
                    cells.fill(Role::Plain, ' ');
                    return;
                };
                cells.put(Role::FileHeader, &String::from_utf8_lossy(&c.path));
                if text_matches && i < defs {
                    cells.put(Role::Dim, " · text match");
                } else if c.changed {
                    cells.put(Role::Dim, " · changed");
                }
                cells.fill(Role::Plain, ' ');
            }
            Some(Item::Candidate(i)) => {
                let Some(cand) = self.get(i) else {
                    cells.fill(Role::Plain, ' ');
                    return;
                };
                let selected = i == self.cursor;
                let ink = |role: Role| -> Ink {
                    if selected {
                        Role::Selection.into()
                    } else {
                        role.into()
                    }
                };
                cells.put(ink(Role::LineNumber), &format!("{:>5}  ", cand.start + 1));
                let text = self.line(i).cloned();
                let shown = match (&text, cand.kind) {
                    (Some(text), _) => text.clone(),
                    (None, Some(kind)) => format!("{kind:?}").to_lowercase(),
                    (None, None) => "text match".to_owned(),
                };
                cells.put(ink(Role::Plain), &shown);
                if let Some(container) = &cand.container {
                    cells.put(ink(Role::Dim), &format!("  in {container}"));
                }
                // What the asking file itself defines the name as, past
                // what the line shows.
                let local = cand.kind.filter(|k| {
                    matches!(
                        k,
                        DefinitionKind::Local | DefinitionKind::Parameter | DefinitionKind::Import
                    )
                });
                if let Some(kind) = local {
                    cells.put(ink(Role::Dim), &format!("  {}", kind.name()));
                }
                let more = (self.refs.as_ref())
                    .zip(i.checked_sub(defs))
                    .and_then(|(r, u)| {
                        let k = r.more.binary_search_by_key(&u, |&(at, _)| at).ok()?;
                        Some(r.more[k].1)
                    });
                if let Some(more) = more {
                    cells.put(
                        ink(Role::Dim),
                        &format!("  +{} more in this file", thousands(more)),
                    );
                }
                cells.fill(ink(Role::Plain), ' ');
            }
            None => cells.fill(Role::Plain, ' '),
        }
    }
}

//! The menu a right click opens over what it was on: a file in the tree,
//! a line of code, a `⋯` separator, a comment thread or a peek. The mouse is a
//! shortcut layer, so nothing lives only here: each entry is something a
//! key does there, shown with that key, and choosing it does what the key
//! would. The viewer aims at the target first, as a left click would.

use dffy_core::document::Pos;
use dffy_core::view::Attributes;

use super::help;
use crate::columns::Columns as _;
use crate::keys::{Action as Act, Table};
use crate::term::{Grid, Key, Mods};
use crate::theme::{Role, Style, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Run {
    /// The action, as its key runs it in the pane aimed at.
    Action(Act),
    /// `v` then `y` on the line aimed at.
    CopyLine,
    /// An action of the thread's table, on the thread aimed at.
    Thread(Act),
    /// `x`, on this separator rather than the first on screen.
    Reveal(Pos),
    /// An action of the peek's table, on the peek.
    Peek(Act),
}

/// Something a key does to what it's aimed at, which this menu and the
/// command panel's Here tab list: its words, the action, and the table
/// whose key does it there (a thread's runs on the thread aimed at).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Offer {
    pub label: String,
    pub action: Act,
    pub table: Table,
}

impl Offer {
    pub(super) fn new(label: &str, action: Act, table: Table) -> Self {
        Self {
            label: label.to_owned(),
            action,
            table,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    pub label: String,
    /// Empty when nothing is bound to it.
    pub key: String,
    pub run: Run,
}

pub(super) enum Outcome {
    Stay,
    Close,
    Run(Run),
}

#[derive(Debug)]
pub(super) struct Menu {
    /// The cell right-clicked: the menu's top left corner, moved in as
    /// far as the screen needs.
    at: (usize, usize),
    entries: Vec<Entry>,
    cursor: usize,
}

impl Menu {
    /// `entries` has at least one: the viewer opens no empty menu.
    pub(super) fn new(at: (usize, usize), entries: Vec<Entry>) -> Self {
        Self {
            at,
            entries,
            cursor: 0,
        }
    }

    /// Gives the entries `label` has new words for those.
    pub(super) fn relabel(&mut self, label: impl Fn(Run) -> Option<String>) {
        for entry in &mut self.entries {
            if let Some(label) = label(entry.run) {
                entry.label = label;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub(super) fn key(&mut self, key: Key, mods: Mods) -> Outcome {
        let len = self.entries.len();
        match key {
            Key::Esc => return Outcome::Close,
            Key::Char('c') if mods.contains(Mods::CTRL) => return Outcome::Close,
            Key::Enter => return Outcome::Run(self.entries[self.cursor].run),
            Key::Down | Key::Char('j') => self.cursor = (self.cursor + 1) % len,
            Key::Up | Key::Char('k') => self.cursor = (self.cursor + len - 1) % len,
            _ => {}
        }
        Outcome::Stay
    }

    /// A tap at cell `(x, y)` of a `size` screen: an entry runs; on the
    /// border nothing happens; outside, the menu closes.
    pub(super) fn tap(&mut self, (x, y): (usize, usize), size: (usize, usize)) -> Outcome {
        let (x0, y0, width, height) = self.rect(size);
        if !(x0..x0 + width).contains(&x) || !(y0..y0 + height).contains(&y) {
            return Outcome::Close;
        }
        match (y - y0).checked_sub(1) {
            Some(i) if i < self.entries.len() && y + 1 < y0 + height => {
                self.cursor = i;
                Outcome::Run(self.entries[i].run)
            }
            _ => Outcome::Stay,
        }
    }

    fn widths(&self) -> (usize, usize) {
        let widest =
            |f: fn(&Entry) -> &str| self.entries.iter().map(|e| f(e).width()).max().unwrap_or(0);
        (widest(|e| &e.label), widest(|e| &e.key))
    }

    /// Where it's drawn on a `(width, height)` screen: its corner, width
    /// and height.
    fn rect(&self, (gw, gh): (usize, usize)) -> (usize, usize, usize, usize) {
        let (labels, keys) = self.widths();
        let gap = if keys > 0 { 3 } else { 0 };
        // Border and a space of margin on either side.
        let width = (labels + gap + keys + 4).min(gw);
        let height = (self.entries.len() + 2).min(gh);
        (
            self.at.0.min(gw - width),
            self.at.1.min(gh - height),
            width,
            height,
        )
    }

    pub(super) fn draw(&self, grid: &mut Grid, theme: &Theme) {
        let (x0, y0, width, height) = self.rect((grid.width(), grid.height()));
        if width < 5 || height < 3 {
            return;
        }
        help::border(grid, (x0, y0, width, height), theme);
        let (x, end) = (x0 + 2, x0 + width - 2);
        let key_x = (x + self.widths().0 + 3).min(end);
        for (i, e) in self.entries.iter().enumerate().take(height - 2) {
            let y = y0 + 1 + i;
            let selected = i == self.cursor;
            let mark = |s: Style| {
                if selected { theme.selected(s, true) } else { s }
            };
            let mut label = mark(Style::PLAIN);
            if selected {
                grid.fill(x0 + 1, y, x0 + width - 1, label);
                label.attributes |= Attributes::BOLD;
            }
            grid.put_str(x, y, key_x - 1, &e.label, label);
            grid.put_str(key_x, y, end, &e.key, mark(theme.style(Role::Hint)));
        }
    }
}

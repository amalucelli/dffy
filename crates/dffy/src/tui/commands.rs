//! The command panel the leader key (`space`) opens: the viewer's toggles
//! and a few actions, one per row with its state and its key, grouped.
//! A row's key runs it straight away; or `j`/`k` move a cursor and `enter`
//! runs the row under it. Toggles keep the panel open and flash their new
//! state; actions that go somewhere (the picker, the next file) close it.
//! Settings with a few values show `‹ value ›`: `h`/`l`, the arrows or a
//! tap on either mark step back or forward; on other rows `h`/`l` do
//! nothing, so no entry can have them as its key. The theme row opens a
//! list of every theme instead, which tries each one on the screen behind
//! it as the cursor moves. `/` filters by label or key. On a narrow screen
//! it takes the whole screen.

use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

use super::{fuzzy, help};
use crate::keys::{Action, Group};
use crate::term::{Grid, Key, Mods};
use crate::theme::{Role, Style, Theme};

const FULL_SCREEN_BELOW: usize = 70;
const TITLE: &str = " commands ";
const FOOTER: &str = "key or enter runs · j/k move · / search · esc close";
const THEMES_TITLE: &str = " themes ";
const THEMES_FOOTER: &str = "type to filter · ↑/↓ try · enter keep · esc back";
/// How long a toggled row's new state stands out.
pub(super) const FLASH: Duration = Duration::from_millis(600);

/// A row of the panel as the viewer describes it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    pub action: Action,
    /// The key that runs it from the panel.
    pub key: String,
    /// A toggle's state; `None` for actions and multi-value settings.
    pub on: Option<bool>,
    pub label: String,
    /// A multi-value setting's value (`auto (split here)`).
    pub value: Option<String>,
    /// The key that does the same outside the panel, when it's another.
    pub direct: Option<String>,
    /// Running it keeps the panel open (a setting) or closes it.
    pub stays: bool,
}

/// What a key or a tap in the panel asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    Stay,
    Close,
    /// Run this entry's action; `back` cycles a multi-value one backward.
    Run {
        action: Action,
        back: bool,
    },
    /// A key no entry has.
    Unknown,
    /// Open the theme list: the viewer hands it the themes with
    /// [`Commands::open_themes`].
    OpenThemes,
    /// Show this theme, and with `keep` make it the choice; the list
    /// closes on `keep` and when `esc` brings back the theme it opened on.
    Theme {
        name: String,
        keep: bool,
    },
}

#[derive(Debug, Default)]
pub(super) struct Commands {
    /// The entry under the cursor, among those the filter keeps.
    cursor: usize,
    scroll: usize,
    filter: String,
    editing: bool,
    /// Rows the list showed last, for paging and taps.
    page: usize,
    /// Where the list's first row was drawn, for taps.
    top: usize,
    /// The action whose row flashes, and since when.
    flash: Option<(Action, Instant)>,
    /// The `‹` and `›` of each value drawn: screen row, then their columns.
    arrows: Vec<(usize, usize, usize)>,
    /// The theme list, while it's open over the panel.
    themes: Option<ThemeList>,
}

/// A row of the list as drawn: a group's title, or an entry by its index.
enum Line {
    Title(&'static str),
    Entry(usize),
    Blank,
}

fn title(group: Group) -> &'static str {
    match group {
        Group::Navigation => "Move",
        Group::Files => "Files",
        Group::View => "View",
        Group::Copy => "Copy",
        Group::Modes => "Modes",
        Group::Other => "Help",
    }
}

impl Commands {
    /// The entries the filter keeps with the label's matched characters:
    /// all of them by group, then as the leader's table lists them; with
    /// a filter, the best matches of label or key first.
    fn kept<'a>(&self, entries: &'a [Entry]) -> Vec<(&'a Entry, Vec<usize>)> {
        if self.filter.is_empty() {
            let mut kept: Vec<(&Entry, Vec<usize>)> =
                entries.iter().map(|e| (e, Vec::new())).collect();
            kept.sort_by_key(|(e, _)| e.action.group());
            return kept;
        }
        let mut scored: Vec<(i64, &Entry, Vec<usize>)> = entries
            .iter()
            .filter_map(|e| {
                let label = fuzzy::score_close(&self.filter, &e.label);
                // A key typed whole is the best match there is.
                let key = (e.key.to_lowercase() == self.filter).then_some(1000);
                match (label, key) {
                    (Some((s, at)), key) => Some((s.max(key.unwrap_or(0)), e, at)),
                    (None, Some(s)) => Some((s, e, Vec::new())),
                    (None, None) => None,
                }
            })
            .collect();
        scored.sort_by_key(|&(s, ..)| std::cmp::Reverse(s));
        scored.into_iter().map(|(_, e, at)| (e, at)).collect()
    }

    /// The rows: groups with titles, or with a filter, the matches alone.
    fn lines(&self, kept: &[(&Entry, Vec<usize>)]) -> Vec<Line> {
        if !self.filter.is_empty() {
            return (0..kept.len()).map(Line::Entry).collect();
        }
        let mut out = Vec::new();
        let mut group = None;
        for (i, (e, _)) in kept.iter().enumerate() {
            if group != Some(e.action.group()) {
                if group.is_some() {
                    out.push(Line::Blank);
                }
                group = Some(e.action.group());
                out.push(Line::Title(title(e.action.group())));
            }
            out.push(Line::Entry(i));
        }
        out
    }

    #[cfg(test)]
    pub(super) fn cursor(&self) -> usize {
        self.cursor
    }

    /// The action of row `i` among those the filter keeps.
    #[cfg(test)]
    pub(super) fn entry(&self, entries: &[Entry], i: usize) -> Option<Action> {
        self.kept(entries).get(i).map(|(e, _)| e.action)
    }

    pub(super) fn flash(&mut self, action: Action) {
        self.flash = Some((action, Instant::now()));
    }

    /// When the flash ends, for a redraw then.
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.flash.map(|(_, at)| at + FLASH)
    }

    /// Opens the theme list on `names`, each with its [`crate::theme::GROUPS`]
    /// index, the cursor on `current`.
    pub(super) fn open_themes(&mut self, names: Vec<(String, usize)>, current: &str) {
        let mut list = ThemeList {
            names,
            original: current.to_owned(),
            ..ThemeList::default()
        };
        list.cursor = list
            .kept()
            .iter()
            .position(|(i, _)| list.names[*i].0 == current)
            .unwrap_or(0);
        self.themes = Some(list);
    }

    #[cfg(test)]
    pub(super) fn themes_open(&self) -> bool {
        self.themes.is_some()
    }

    /// What running `e` from the cursor or a tap asks for: the theme row
    /// opens the list; every other row runs.
    fn run(e: &Entry, back: bool) -> Outcome {
        if e.action == Action::CycleTheme && !back {
            return Outcome::OpenThemes;
        }
        Outcome::Run {
            action: e.action,
            back,
        }
    }

    pub(super) fn key(&mut self, entries: &[Entry], key: Key, mods: Mods) -> Outcome {
        if let Some(list) = &mut self.themes {
            let outcome = list.key(key, mods);
            if matches!(outcome, Outcome::Theme { keep: true, .. }) || list.closing {
                self.themes = None;
            }
            return outcome;
        }
        let kept: Vec<&Entry> = self.kept(entries).into_iter().map(|(e, _)| e).collect();
        if self.editing {
            match key {
                Key::Esc => {
                    self.filter.clear();
                    self.editing = false;
                }
                Key::Enter => self.editing = false,
                Key::Backspace => {
                    self.filter.pop();
                }
                Key::Char(c) if !mods.contains(Mods::CTRL) => self.filter.extend(c.to_lowercase()),
                _ => {}
            }
            self.cursor = 0;
            return Outcome::Stay;
        }
        let page = self.page.max(1);
        let len = kept.len();
        let on_value = kept.get(self.cursor).is_some_and(|e| e.value.is_some());
        let step = |back: bool| match kept.get(self.cursor) {
            Some(e) => Outcome::Run {
                action: e.action,
                back,
            },
            None => Outcome::Stay,
        };
        let step_or_stay = |back: bool| if on_value { step(back) } else { Outcome::Stay };
        match key {
            // Off a value these do nothing rather than fall through to a
            // row bound to the same letter.
            Key::Left | Key::Char('h') if mods == Mods::NONE => return step_or_stay(true),
            Key::Right | Key::Char('l') if mods == Mods::NONE => return step_or_stay(false),
            Key::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.cursor = 0;
            }
            Key::Esc => return Outcome::Close,
            Key::Char('c') if mods.contains(Mods::CTRL) => return Outcome::Close,
            Key::Char('/') => self.editing = true,
            Key::Enter | Key::Char(' ') => {
                return kept
                    .get(self.cursor)
                    .map_or(Outcome::Stay, |e| Self::run(e, false));
            }
            // Kept from before the `‹ ›` marks, as a hidden alias of `h`.
            Key::Backspace if on_value => return step(true),
            // Past either end, the cursor goes round.
            Key::Down | Key::Char('j') if len > 0 => self.cursor = (self.cursor + 1) % len,
            Key::Up | Key::Char('k') if len > 0 => self.cursor = (self.cursor + len - 1) % len,
            Key::PageDown => self.cursor = (self.cursor + page).min(len.saturating_sub(1)),
            Key::PageUp => self.cursor = self.cursor.saturating_sub(page),
            _ => {
                let name = crate::keys::Chord { key, mods }.name();
                return match kept.iter().position(|e| e.key == name) {
                    // A row's own key steps it forward, theme included.
                    Some(i) => {
                        self.cursor = i;
                        Outcome::Run {
                            action: kept[i].action,
                            back: false,
                        }
                    }
                    None => Outcome::Unknown,
                };
            }
        }
        Outcome::Stay
    }

    /// The wheel moves the cursor.
    pub(super) fn scroll(&mut self, entries: &[Entry], rows: isize) -> Outcome {
        if let Some(list) = &mut self.themes {
            return list.scroll(rows);
        }
        let len = self.kept(entries).len();
        self.cursor = self
            .cursor
            .saturating_add_signed(rows)
            .min(len.saturating_sub(1));
        Outcome::Stay
    }

    /// A tap at screen cell `(x, y)`: on a value's `‹` or `›` it steps the
    /// value; elsewhere on a row, the entry runs; outside the list, the
    /// panel closes.
    pub(super) fn tap(&mut self, entries: &[Entry], (x, y): (usize, usize)) -> Outcome {
        if let Some(list) = &mut self.themes {
            let outcome = list.tap(y);
            if matches!(outcome, Outcome::Theme { keep: true, .. }) || list.closing {
                self.themes = None;
            }
            return outcome;
        }
        let kept = self.kept(entries);
        let arrow = self
            .arrows
            .iter()
            .find(|&&(row, ..)| row == y)
            .and_then(|&(_, left, right)| (x == left || x == right).then_some(x == left));
        let lines = self.lines(&kept);
        let Some(row) = y.checked_sub(self.top).filter(|&r| r < self.page) else {
            return Outcome::Close;
        };
        match lines.get(self.scroll + row) {
            Some(&Line::Entry(i)) => {
                self.cursor = i;
                match arrow {
                    Some(back) => Outcome::Run {
                        action: kept[i].0.action,
                        back,
                    },
                    None => Self::run(kept[i].0, false),
                }
            }
            _ => Outcome::Stay,
        }
    }

    pub(super) fn draw(&mut self, grid: &mut Grid, theme: &Theme, entries: &[Entry]) {
        let (gw, gh) = (grid.width(), grid.height());
        if gw < 12 || gh < 6 {
            return;
        }
        if let Some(list) = &mut self.themes {
            list.draw(grid, theme);
            return;
        }
        let kept = self.kept(entries);
        let lines = self.lines(&kept);
        self.cursor = self.cursor.min(kept.len().saturating_sub(1));
        // Sized for every entry, so filtering doesn't resize the panel.
        let keys_width = entries
            .iter()
            .map(|e| e.key.width() + e.direct.as_ref().map_or(0, |d| d.width() + 1))
            .max()
            .unwrap_or(0);
        let labels_width = entries
            .iter()
            .map(|e| e.label.width() + e.value.as_ref().map_or(0, |v| v.width() + 5))
            .max()
            .unwrap_or(0);
        let full = gw < FULL_SCREEN_BELOW;
        // Border, margins, state, label, gap, keys, scrollbar.
        let wanted = (2 + 2 + 4 + labels_width + 3 + keys_width + 2).max(FOOTER.width() + 6);
        let width = if full { gw } else { wanted.clamp(44, gw - 4) };
        let height = if full {
            gh
        } else {
            (lines.len() + 4).min(gh - 2)
        };
        let (x0, y0) = ((gw - width) / 2, (gh - height) / 2);
        let end = x0 + width;
        let border = theme.style(Role::Dim);
        help::frame(grid, (x0, y0, width, height), TITLE, theme);
        let rows = height.saturating_sub(3);
        self.page = rows;
        self.top = y0 + 1;
        let at = lines
            .iter()
            .position(|l| matches!(l, Line::Entry(i) if *i == self.cursor))
            .unwrap_or(0);
        if at < self.scroll {
            self.scroll = at.saturating_sub(1);
        } else if at >= self.scroll + rows {
            self.scroll = at + 1 - rows;
        }
        self.scroll = self.scroll.min(lines.len().saturating_sub(rows));
        let x = x0 + 2;
        let right = end - 3;
        let key_x = right.saturating_sub(keys_width);
        let flashing = self
            .flash
            .filter(|(_, at)| at.elapsed() < FLASH)
            .map(|(a, _)| a);
        self.arrows.clear();
        for (i, line) in lines.iter().skip(self.scroll).take(rows).enumerate() {
            let y = y0 + 1 + i;
            match *line {
                Line::Title(text) => {
                    let mut bold = theme.style(Role::Plain);
                    bold.attributes |= Style::BOLD;
                    grid.put_str(x, y, right, text, bold);
                }
                Line::Entry(k) => {
                    let (e, matched) = &kept[k];
                    let row = Row {
                        selected: k == self.cursor,
                        flashing: flashing == Some(e.action),
                    };
                    if let Some(arrows) = row.draw(grid, theme, (x, y, key_x, right), e, matched) {
                        self.arrows.push((y, arrows.0, arrows.1));
                    }
                }
                Line::Blank => {}
            }
        }
        help::scrollbar(
            grid,
            (end - 2, y0 + 1, rows),
            (lines.len(), self.scroll),
            border,
        );
        let footer = if self.editing || !self.filter.is_empty() {
            let cursor = if self.editing { "▏" } else { "" };
            format!("/{}{cursor}  · clear esc", self.filter)
        } else if kept.is_empty() {
            "nothing matches".to_owned()
        } else {
            FOOTER.to_owned()
        };
        grid.put_str(x, y0 + height - 2, right, &footer, border);
    }
}

struct Row {
    /// Under the cursor: in the theme's selection, the label bold.
    selected: bool,
    /// Just toggled: its state stands out.
    flashing: bool,
}

impl Row {
    /// `[x] Label ‹ value › ... key direct` between `x` and `right`, the
    /// key at `key_x`, the label's `matched` characters marked. Returns
    /// the columns of the value's `‹` and `›`, when it has one.
    fn draw(
        &self,
        grid: &mut Grid,
        theme: &Theme,
        (x, y, key_x, right): (usize, usize, usize, usize),
        e: &Entry,
        matched: &[usize],
    ) -> Option<(usize, usize)> {
        let mark = |s: Style| {
            if self.selected {
                theme.selected(s, true)
            } else {
                s
            }
        };
        if self.selected {
            grid.fill(x - 1, y, right + 1, mark(Style::PLAIN));
        }
        let state = match e.on {
            Some(true) => "[x]",
            Some(false) => "[ ]",
            None => "   ",
        };
        let style = if self.flashing {
            theme.style(Role::Hint)
        } else {
            Style::PLAIN
        };
        let after = grid.put_str(x, y, right, state, mark(style));
        let bold = |mut s: Style| {
            if self.selected {
                s.attributes |= Style::BOLD;
            }
            s
        };
        let label_end = key_x.saturating_sub(1);
        fuzzy::put_marked(
            grid,
            (after + 1, y, label_end),
            &e.label,
            matched,
            (bold(mark(style)), bold(mark(theme.style(Role::Hint)))),
        );
        let arrows = e.value.as_ref().map(|value| {
            let left = after + 1 + e.label.width() + 1;
            let dim = mark(theme.style(Role::Dim));
            let at = grid.put_str(left, y, label_end, "‹ ", dim);
            let at = grid.put_str(at, y, label_end, value, mark(style));
            grid.put_str(at, y, label_end, " ›", dim);
            (left, at + 1)
        });
        let after = grid.put_str(key_x, y, right, &e.key, mark(theme.style(Role::Hint)));
        if let Some(direct) = &e.direct {
            grid.put_str(after + 1, y, right, direct, mark(theme.style(Role::Dim)));
        }
        arrows
    }
}

/// The list of themes the theme row opens: grouped dark and light, fuzzy
/// filtered by what's typed, the theme under the cursor shown live.
#[derive(Debug, Default)]
struct ThemeList {
    /// Every theme with its group, in the order the panel cycles them.
    names: Vec<(String, usize)>,
    /// The theme the list opened on, which `esc` brings back.
    original: String,
    filter: String,
    /// Among the themes the filter keeps.
    cursor: usize,
    scroll: usize,
    page: usize,
    top: usize,
    /// Set by a key that closes the list without keeping a theme.
    closing: bool,
}

impl ThemeList {
    /// The themes the filter keeps, as indices into `names` with the
    /// matched characters: by group without a filter, best match first
    /// with one.
    fn kept(&self) -> Vec<(usize, Vec<usize>)> {
        if self.filter.is_empty() {
            let mut kept: Vec<(usize, Vec<usize>)> =
                (0..self.names.len()).map(|i| (i, Vec::new())).collect();
            kept.sort_by_key(|&(i, _)| self.names[i].1);
            return kept;
        }
        let mut scored: Vec<(i64, usize, Vec<usize>)> = self
            .names
            .iter()
            .enumerate()
            .filter_map(|(i, (name, _))| {
                fuzzy::score_close(&self.filter, name).map(|(s, at)| (s, i, at))
            })
            .collect();
        scored.sort_by_key(|&(s, ..)| std::cmp::Reverse(s));
        scored.into_iter().map(|(_, i, at)| (i, at)).collect()
    }

    /// The rows: group titles and themes, or with a filter, the matches.
    fn lines(&self, kept: &[(usize, Vec<usize>)]) -> Vec<Line> {
        if !self.filter.is_empty() {
            return (0..kept.len()).map(Line::Entry).collect();
        }
        let mut out = Vec::new();
        let mut group = None;
        for (k, &(i, _)) in kept.iter().enumerate() {
            let g = self.names[i].1;
            if group != Some(g) {
                if group.is_some() {
                    out.push(Line::Blank);
                }
                group = Some(g);
                out.push(Line::Title(crate::theme::GROUPS[g]));
            }
            out.push(Line::Entry(k));
        }
        out
    }

    /// The theme under the cursor, shown, when the cursor moved off
    /// `before`.
    fn moved(&self, before: Option<&str>) -> Outcome {
        match self.current() {
            Some(name) if Some(name.as_str()) != before => Outcome::Theme { name, keep: false },
            _ => Outcome::Stay,
        }
    }

    fn current(&self) -> Option<String> {
        self.kept()
            .get(self.cursor)
            .map(|&(i, _)| self.names[i].0.clone())
    }

    fn back(&mut self) -> Outcome {
        self.closing = true;
        Outcome::Theme {
            name: self.original.clone(),
            keep: false,
        }
    }

    fn key(&mut self, key: Key, mods: Mods) -> Outcome {
        let before = self.current();
        let len = self.kept().len();
        let ctrl = mods.contains(Mods::CTRL);
        match key {
            Key::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.cursor = self
                    .kept()
                    .iter()
                    .position(|&(i, _)| Some(&self.names[i].0) == before.as_ref())
                    .unwrap_or(0);
            }
            Key::Esc => return self.back(),
            Key::Char('c') if ctrl => return self.back(),
            Key::Enter => {
                return match before {
                    Some(name) => Outcome::Theme { name, keep: true },
                    None => Outcome::Stay,
                };
            }
            Key::Down if len > 0 => self.cursor = (self.cursor + 1) % len,
            Key::Char('n') if ctrl && len > 0 => self.cursor = (self.cursor + 1) % len,
            Key::Up if len > 0 => self.cursor = (self.cursor + len - 1) % len,
            Key::Char('p') if ctrl && len > 0 => self.cursor = (self.cursor + len - 1) % len,
            Key::PageDown => {
                self.cursor = (self.cursor + self.page.max(1)).min(len.saturating_sub(1));
            }
            Key::PageUp => self.cursor = self.cursor.saturating_sub(self.page.max(1)),
            Key::Backspace => {
                self.filter.pop();
                self.cursor = 0;
            }
            Key::Char(c) if !ctrl => {
                self.filter.extend(c.to_lowercase());
                self.cursor = 0;
            }
            _ => {}
        }
        self.moved(before.as_deref())
    }

    fn scroll(&mut self, rows: isize) -> Outcome {
        let before = self.current();
        let len = self.kept().len();
        self.cursor = self
            .cursor
            .saturating_add_signed(rows)
            .min(len.saturating_sub(1));
        self.moved(before.as_deref())
    }

    /// A tap on a theme keeps it; outside the list, the list closes and
    /// the theme it opened on comes back.
    fn tap(&mut self, y: usize) -> Outcome {
        let kept = self.kept();
        let lines = self.lines(&kept);
        let Some(row) = y.checked_sub(self.top).filter(|&r| r < self.page) else {
            return self.back();
        };
        match lines.get(self.scroll + row) {
            Some(&Line::Entry(k)) => {
                self.cursor = k;
                Outcome::Theme {
                    name: self.names[kept[k].0].0.clone(),
                    keep: true,
                }
            }
            _ => Outcome::Stay,
        }
    }

    fn draw(&mut self, grid: &mut Grid, theme: &Theme) {
        let (gw, gh) = (grid.width(), grid.height());
        let kept = self.kept();
        let lines = self.lines(&kept);
        self.cursor = self.cursor.min(kept.len().saturating_sub(1));
        let names_width = self.names.iter().map(|(n, _)| n.width()).max().unwrap_or(0);
        let full = gw < FULL_SCREEN_BELOW;
        let width = if full {
            gw
        } else {
            (names_width + 10)
                .max(THEMES_FOOTER.width() + 6)
                .clamp(30, gw - 4)
        };
        // Sized for every theme, so filtering doesn't resize the list.
        // Each group's title, and a blank line between groups.
        let all = self.names.len() + 2 * crate::theme::GROUPS.len() - 1;
        let height = if full { gh } else { (all + 4).min(gh - 2) };
        let (x0, y0) = ((gw - width) / 2, (gh - height) / 2);
        let end = x0 + width;
        let border = theme.style(Role::Dim);
        help::frame(grid, (x0, y0, width, height), THEMES_TITLE, theme);
        let rows = height.saturating_sub(3);
        self.page = rows;
        self.top = y0 + 1;
        let at = lines
            .iter()
            .position(|l| matches!(l, Line::Entry(k) if *k == self.cursor))
            .unwrap_or(0);
        if at < self.scroll {
            self.scroll = at.saturating_sub(1);
        } else if at >= self.scroll + rows {
            self.scroll = at + 1 - rows;
        }
        self.scroll = self.scroll.min(lines.len().saturating_sub(rows));
        let (x, right) = (x0 + 2, end - 3);
        for (row, line) in lines.iter().skip(self.scroll).take(rows).enumerate() {
            let y = y0 + 1 + row;
            match *line {
                Line::Title(text) => {
                    let mut bold = theme.style(Role::Plain);
                    bold.attributes |= Style::BOLD;
                    grid.put_str(x, y, right, text, bold);
                }
                Line::Entry(entry) => {
                    let (index, matched) = &kept[entry];
                    let name = &self.names[*index].0;
                    let selected = entry == self.cursor;
                    let mark = |s: Style| {
                        if selected {
                            let mut s = theme.selected(s, true);
                            s.attributes |= Style::BOLD;
                            s
                        } else {
                            s
                        }
                    };
                    if selected {
                        grid.fill(x - 1, y, right + 1, theme.selected(Style::PLAIN, true));
                    }
                    let current = if *name == self.original { "* " } else { "  " };
                    let after = grid.put_str(x, y, right, current, mark(theme.style(Role::Ok)));
                    fuzzy::put_marked(
                        grid,
                        (after, y, right),
                        name,
                        matched,
                        (mark(Style::PLAIN), mark(theme.style(Role::Hint))),
                    );
                }
                Line::Blank => {}
            }
        }
        help::scrollbar(
            grid,
            (end - 2, y0 + 1, rows),
            (lines.len(), self.scroll),
            border,
        );
        let footer = if self.filter.is_empty() {
            THEMES_FOOTER.to_owned()
        } else if kept.is_empty() {
            format!("/{}▏  · no theme matches", self.filter)
        } else {
            format!("/{}▏  · clear esc", self.filter)
        };
        grid.put_str(x, y0 + height - 2, right, &footer, border);
    }
}

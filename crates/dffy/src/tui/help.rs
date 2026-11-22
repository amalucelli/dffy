//! The `?` overlay: the key bindings in effect, the config's included, in
//! seven tabs shown one at a time, a tab of several groups titling each.
//! Each action gets a row in each table it's bound in (the diff's, and the
//! tree's, a thread's and a conflict's under titles of their own): its
//! first key there, at most one other (dimmed), and a few words. Actions
//! without a key are left out, since the command panel reaches them;
//! docs/keys.md has every key and the full descriptions. `/` searches all
//! tabs at once. A wide screen gets a tab's rows in two columns; a narrow
//! one (a phone) gets the whole screen and one column, scrolled with
//! `j`/`k`.

use std::ops::Range;

use dffy_core::view::Attributes;
use unicode_width::UnicodeWidthStr;

use super::fuzzy;
use crate::keys::{Action, Chord, Keys, Tab};
use crate::term::{Grid, Key, Mods};
use crate::theme::{Role, Style, Theme};

/// Narrower screens get the whole screen.
pub(super) const FULL_SCREEN_BELOW: usize = 70;
const TITLE: &str = " keybinds ";
const CLOSE: &str = " esc close ";
pub(super) const HINTS: &str = "tab section · / search · esc close";
const TABS: [Tab; 7] = [
    Tab::Move,
    Tab::Files,
    Tab::View,
    Tab::Git,
    Tab::Copy,
    Tab::Review,
    Tab::General,
];
/// Between a row's keys and its label.
const GAP: usize = 2;
const COLUMN_GAP: usize = 4;
/// The border and a column of margin each side, and the scrollbar's column.
const CHROME: usize = 5;

#[derive(Debug, Default)]
pub(super) struct Help {
    tab: usize,
    scroll: usize,
    /// Typed after `/`, lowercase.
    filter: String,
    /// Keys go to the filter.
    editing: bool,
    /// Rows the list showed last, for paging.
    page: usize,
    /// The screen row the tab titles were drawn on last, and the columns of
    /// each, for taps.
    titles: (usize, Vec<Range<usize>>),
}

fn tab(tab: Tab) -> usize {
    TABS.iter().position(|&t| t == tab).unwrap_or(0)
}

/// The table a row's keys are from: the diff's, or one that comes first
/// somewhere, listed under its own title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    Main,
    Tree,
    Thread,
    Conflict,
}

impl Section {
    const TITLED: [Self; 3] = [Self::Tree, Self::Thread, Self::Conflict];

    fn title(self) -> &'static str {
        match self {
            Self::Main => "",
            Self::Tree => "in the tree",
            Self::Thread => "on a thread",
            Self::Conflict => "in a conflict",
        }
    }
}

/// An action's row: in its group's tab, or for keys from the tree's own
/// table, in the Files tab's tree group; a thread's keys in a group of
/// their own in the Review tab, and a conflict's in the Git tab.
struct Row {
    action: Action,
    tab: usize,
    section: Section,
    key: String,
    alias: Option<String>,
}

impl Row {
    fn keys(&self) -> String {
        match &self.alias {
            Some(alias) => format!("{} {alias}", self.key),
            None => self.key.clone(),
        }
    }
}

/// Which of an action's other keys shows beside its first: plain letters
/// read best, then named keys, then other characters, then combinations
/// with ctrl or alt.
fn rank(chord: Chord) -> u8 {
    match chord.key {
        _ if chord.mods.contains(Mods::CTRL) || chord.mods.contains(Mods::ALT) => 3,
        Key::Char(c) if c.is_alphabetic() => 0,
        Key::Char(c) if c != ' ' => 2,
        _ => 1,
    }
}

/// Every action with a key, tab by tab, each tab's titled groups last.
fn rows(keys: &Keys) -> Vec<Row> {
    let sections = [
        Section::Main,
        Section::Tree,
        Section::Thread,
        Section::Conflict,
    ];
    let mut rows: Vec<Row> = Action::ALL
        .iter()
        .flat_map(|&action| {
            sections.into_iter().filter_map(move |section| {
                let mut chords: Box<dyn Iterator<Item = Chord>> = match section {
                    Section::Main => Box::new(keys.chords(false, action)),
                    Section::Tree => Box::new(keys.chords(true, action)),
                    Section::Thread => Box::new(keys.thread_chords(action)),
                    Section::Conflict => Box::new(keys.conflict_chords(action)),
                };
                let first = chords.next()?;
                Some(Row {
                    action,
                    tab: tab(match section {
                        Section::Main => action.group().tab(),
                        Section::Tree => Tab::Files,
                        Section::Thread => Tab::Review,
                        Section::Conflict => Tab::Git,
                    }),
                    section,
                    key: first.name(),
                    alias: chords.min_by_key(|&c| rank(c)).map(Chord::name),
                })
            })
        })
        .collect();
    rows.sort_by_key(|r| {
        let group = (r.section == Section::Main).then(|| r.action.group());
        (r.tab, r.section, group)
    });
    rows
}

enum Line<'a> {
    Title(&'static str),
    /// A row, with the search's matched characters in its keys and label.
    Entry(&'a Row, Vec<usize>, Vec<usize>),
    Note(&'static str),
    Blank,
}

impl Line<'_> {
    /// Draws the line from column `x` of row `y`, left of `end`: an entry's
    /// keys in `key_w` columns, then its label, and with `place_x`, its tab
    /// that far from `x`, or nearer when the row is too short; left out
    /// where it would run into the label.
    fn put(
        &self,
        grid: &mut Grid,
        theme: &Theme,
        (x, y, end): (usize, usize, usize),
        (key_w, place_x): (usize, Option<usize>),
    ) {
        let dim = theme.style(Role::Dim);
        match self {
            Self::Title(text) => {
                let mut bold = theme.style(Role::Plain);
                bold.attributes |= Attributes::BOLD;
                grid.put_str(x, y, end, text, bold);
            }
            Self::Entry(row, key_hits, label_hits) => {
                let mut hit = theme.style(Role::Hint);
                hit.attributes |= Attributes::UNDERLINE;
                let mut keys = row.keys();
                if keys.width() > key_w {
                    keys.clone_from(&row.key);
                }
                let mut at = x;
                for (i, c) in keys.char_indices() {
                    let style = if key_hits.contains(&i) {
                        hit
                    } else if i < row.key.len() {
                        theme.style(Role::Hint)
                    } else {
                        dim
                    };
                    at = grid.put_char(at, y, end, c, style);
                }
                let label_end = fuzzy::put_marked(
                    grid,
                    (x + key_w + GAP, y, end),
                    row.action.short(),
                    label_hits,
                    (Style::PLAIN, hit),
                );
                if let Some(px) = place_x {
                    let place = TABS[row.tab].title();
                    let at = (x + px).min(end.saturating_sub(place.width()));
                    if at > label_end {
                        grid.put_str(at, y, end, place, dim);
                    }
                }
            }
            Self::Note(text) => {
                grid.put_str(x, y, end, text, dim);
            }
            Self::Blank => {}
        }
    }
}

/// Tab `tab`'s lines, in a left and a right column when `two`: the titled
/// groups on the right when the tab has some, else half each, split
/// between groups where it has several. The diff's
/// keys go under their groups' titles when the tab has more than one.
fn columns(rows: &[Row], tab: usize, two: bool) -> (Vec<Line<'_>>, Vec<Line<'_>>) {
    let kept = |section: Section| {
        rows.iter()
            .filter(move |r| r.tab == tab && r.section == section)
            .map(|r| Line::Entry(r, Vec::new(), Vec::new()))
    };
    let main_rows: Vec<&Row> = (rows.iter())
        .filter(|r| r.tab == tab && r.section == Section::Main)
        .collect();
    let titled_groups = main_rows
        .windows(2)
        .any(|w| w[0].action.group() != w[1].action.group());
    let mut main: Vec<Line<'_>> = Vec::new();
    for (i, row) in main_rows.iter().enumerate() {
        let group = row.action.group();
        if titled_groups
            && main_rows[..i]
                .last()
                .is_none_or(|r| r.action.group() != group)
        {
            if i > 0 {
                main.push(Line::Blank);
            }
            main.push(Line::Title(group.title()));
        }
        main.push(Line::Entry(row, Vec::new(), Vec::new()));
    }
    let mut titled: Vec<Line<'_>> = Vec::new();
    for section in Section::TITLED {
        let mut lines = kept(section).peekable();
        if lines.peek().is_none() {
            continue;
        }
        if !titled.is_empty() {
            titled.push(Line::Blank);
        }
        titled.push(Line::Title(section.title()));
        titled.extend(lines);
    }
    if two && !main.is_empty() && !titled.is_empty() {
        return (main, titled);
    }
    if !main.is_empty() && !titled.is_empty() {
        main.push(Line::Blank);
    }
    main.append(&mut titled);
    if two {
        // At the blank between groups nearest half way, which shows in
        // neither column, else half way.
        let half = main.len().div_ceil(2);
        let blank = (0..main.len())
            .filter(|&i| matches!(main[i], Line::Blank))
            .min_by_key(|&i| i.abs_diff(half));
        let right = main.split_off(blank.unwrap_or(half));
        let skip = usize::from(blank.is_some());
        return (main, right.into_iter().skip(skip).collect());
    }
    (main, Vec::new())
}

impl Help {
    /// Returns false when the key closes the overlay.
    pub(super) fn key(&mut self, keys: &Keys, key: Key, mods: Mods) -> bool {
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
                Key::Char(c) if !mods.contains(Mods::CTRL) => {
                    self.filter.extend(c.to_lowercase());
                }
                _ => {}
            }
            self.scroll = 0;
            return true;
        }
        let plain = mods == Mods::NONE;
        match key {
            Key::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.scroll = 0;
                return true;
            }
            Key::Char('/') => {
                self.editing = true;
                return true;
            }
            Key::Tab | Key::Right => {
                self.switch(self.tab + 1);
                return true;
            }
            Key::Char('l') if plain => {
                self.switch(self.tab + 1);
                return true;
            }
            Key::BackTab | Key::Left => {
                self.switch(self.tab + TABS.len() - 1);
                return true;
            }
            Key::Char('h') if plain => {
                self.switch(self.tab + TABS.len() - 1);
                return true;
            }
            Key::Char(c @ '1'..='7') if plain => {
                self.switch("1234567".find(c).unwrap_or(0));
                return true;
            }
            _ => {}
        }
        let page = isize::try_from(self.page.max(1)).unwrap_or(isize::MAX);
        match (key, keys.action(false, key, mods)) {
            (Key::Down, _) | (_, Some(Action::ScrollDown)) => self.scroll_by(1),
            (Key::Up, _) | (_, Some(Action::ScrollUp)) => self.scroll_by(-1),
            (Key::PageDown, _) | (_, Some(Action::PageDown | Action::HalfPageDown)) => {
                self.scroll_by(page);
            }
            (Key::PageUp, _) | (_, Some(Action::PageUp | Action::HalfPageUp)) => {
                self.scroll_by(-page);
            }
            (_, Some(Action::Top)) => self.scroll = 0,
            (_, Some(Action::Bottom)) => self.scroll = usize::MAX,
            _ => return false,
        }
        true
    }

    /// A tap at screen cell `(x, y)`: on a tab's title, that tab.
    pub(super) fn tap(&mut self, x: usize, y: usize) {
        let (row, titles) = &self.titles;
        if y == *row
            && let Some(i) = titles.iter().position(|r| r.contains(&x))
        {
            self.switch(i);
        }
    }

    pub(super) fn scroll_by(&mut self, rows: isize) {
        self.scroll = self.scroll.saturating_add_signed(rows);
    }

    /// Shows tab `tab`, wrapping round, with the search cleared.
    fn switch(&mut self, tab: usize) {
        self.tab = tab % TABS.len();
        self.filter.clear();
        self.editing = false;
        self.scroll = 0;
    }

    /// The search's matches across every tab, best first: by keys, label
    /// or action name, with the matched characters marked where they show.
    /// A key typed whole beats any fuzzy match.
    fn hits<'a>(&self, rows: &'a [Row]) -> Vec<Line<'a>> {
        let mut scored: Vec<(i64, Line<'a>)> = rows
            .iter()
            .filter_map(|row| {
                let on = |text: &str| crate::fuzzy::score_close(&self.filter, text);
                let typed = [Some(&row.key), row.alias.as_ref()]
                    .into_iter()
                    .flatten()
                    .any(|k| k.to_lowercase() == self.filter);
                let key = if typed {
                    Some((1000, Vec::new()))
                } else {
                    on(&row.keys())
                };
                let label = on(row.action.short());
                let name = on(row.action.name());
                let best = [&key, &label, &name]
                    .iter()
                    .filter_map(|s| s.as_ref().map(|(s, _)| *s))
                    .max()?;
                let marks = |s: Option<(i64, Vec<usize>)>| {
                    s.filter(|(s, _)| *s == best)
                        .map(|(_, at)| at)
                        .unwrap_or_default()
                };
                Some((best, Line::Entry(row, marks(key), marks(label))))
            })
            .collect();
        scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
        if scored.is_empty() {
            return vec![Line::Note("no key matches")];
        }
        scored.into_iter().map(|(_, line)| line).collect()
    }

    pub(super) fn draw(&mut self, grid: &mut Grid, theme: &Theme, keys: &Keys) {
        let (gw, gh) = (grid.width(), grid.height());
        if gw < 12 || gh < 6 {
            return;
        }
        let rows = rows(keys);
        let Layout {
            area,
            label_w,
            pair,
            more,
        } = layout(&rows, keys, (gw, gh));
        let Area {
            x0,
            y0,
            width,
            height,
            two,
            ..
        } = area;
        // On a tight screen the labels keep their room and a row's keys
        // drop the alias instead.
        let widest = |f: &dyn Fn(&Row) -> usize| rows.iter().map(f).max().unwrap_or(0);
        let key_w = widest(&|r| r.keys().width())
            .min(width.saturating_sub(CHROME + GAP + label_w))
            .max(widest(&|r| r.key.width()));
        let end = x0 + width;
        let (x, right) = (x0 + 2, end - 3);
        let dim = theme.style(Role::Dim);
        frame(grid, (x0, y0, width, height), TITLE, theme);

        let searching = self.editing || !self.filter.is_empty();
        let titles = (&TABS.map(Tab::title)[..], (self.tab, !searching));
        self.titles = (y0 + 1, tab_strip(grid, theme, (x, y0 + 1, right), titles));
        let (left, right_column) = if self.filter.is_empty() {
            columns(&rows, self.tab, two)
        } else {
            (self.hits(&rows), Vec::new())
        };
        let (top, list_rows) = (area.top(), area.list_rows());
        self.page = list_rows;
        let len = left.len().max(right_column.len());
        self.scroll = self.scroll.min(len.saturating_sub(list_rows));
        let place_x = (!self.filter.is_empty()).then_some(pair + GAP);
        let left_end = if right_column.is_empty() {
            right
        } else {
            x + pair
        };
        for (column_x, column_end, lines) in [
            (x, left_end, &left),
            (x + pair + COLUMN_GAP, right, &right_column),
        ] {
            for (i, line) in lines.iter().skip(self.scroll).take(list_rows).enumerate() {
                line.put(
                    grid,
                    theme,
                    (column_x, top + i, column_end),
                    (key_w, place_x),
                );
            }
        }
        scrollbar(grid, (end - 2, top, list_rows), (len, self.scroll), dim);

        let room = right - x;
        let more = more.iter().find(|m| m.width() <= room).unwrap_or(&more[1]);
        let hints = if searching {
            search_hint(&self.filter, self.editing)
        } else {
            HINTS.to_owned()
        };
        footer(grid, &area, [more, &hints], theme);
    }
}

/// The footer of a search while it's typed (`editing`) or kept.
pub(super) fn search_hint(filter: &str, editing: bool) -> String {
    let cursor = if editing { "▏" } else { "" };
    format!("/{filter}{cursor} · esc clear")
}

/// The box's two footer rows, dimmed.
pub(super) fn footer(grid: &mut Grid, area: &Area, lines: [&str; 2], theme: &Theme) {
    let (x, right) = (area.x0 + 2, area.x0 + area.width - 3);
    let bottom = area.y0 + area.height;
    for (y, line) in [bottom - 3, bottom - 2].into_iter().zip(lines) {
        grid.put_str(x, y, right, line, theme.style(Role::Dim));
    }
}

/// Where the overlay's box goes on the screen, which the command panel
/// shares so the two look one: its corner, size, a blank row above and
/// below the list when there's room, and whether it holds two columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Area {
    pub x0: usize,
    pub y0: usize,
    pub width: usize,
    pub height: usize,
    pub gap: usize,
    pub two: bool,
}

impl Area {
    /// The rows the list has between the tabs and the two footer rows.
    pub(super) fn list_rows(&self) -> usize {
        self.height.saturating_sub(FIXED_ROWS + 2 * self.gap)
    }

    /// The first row of the list.
    pub(super) fn top(&self) -> usize {
        self.y0 + 2 + self.gap
    }
}

/// [`Help::draw`]'s box and the widths it was sized by: the widest label,
/// a column's keys and label, and the footer's pointer to the rest.
struct Layout {
    area: Area,
    label_w: usize,
    pair: usize,
    more: [String; 2],
}

/// Borders, the tabs and two footer rows.
const FIXED_ROWS: usize = 5;

/// The box on a `gw` × `gh` screen of at least 12 × 6, sized by the
/// overlay's rows for the keys in effect.
pub(super) fn area(keys: &Keys, size: (usize, usize)) -> Area {
    layout(&rows(keys), keys, size).area
}

fn layout(rows: &[Row], keys: &Keys, (gw, gh): (usize, usize)) -> Layout {
    let widest = |f: &dyn Fn(&Row) -> usize| rows.iter().map(f).max().unwrap_or(0);
    let label_w = widest(&|r| r.action.short().width());
    let pair = widest(&|r| r.keys().width()) + GAP + label_w;
    let leader = keys.chords(false, Action::Leader).next().map(Chord::name);
    let more = match &leader {
        Some(l) => [
            format!("more settings: {l} · all keys: docs/keys.md"),
            format!("settings: {l} · docs/keys.md"),
        ],
        None => [
            "all keys: docs/keys.md".to_owned(),
            "docs/keys.md".to_owned(),
        ],
    };
    let full = gw < FULL_SCREEN_BELOW;
    let two_width = 2 * pair + COLUMN_GAP + CHROME;
    let two = !full && gw - 4 >= two_width;
    let width = if full {
        gw
    } else if two {
        two_width
    } else {
        (pair + GAP + TABS.iter().map(|t| t.title().width()).max().unwrap_or(0) + CHROME)
            .max(more[0].width() + CHROME)
            .min(gw - 4)
    };
    // Sized for the longest tab, so switching doesn't resize the box.
    let content = (0..TABS.len())
        .map(|t| {
            let (left, right) = columns(rows, t, two);
            left.len().max(right.len())
        })
        .max()
        .unwrap_or(0);
    // A blank row above and below the list when the screen has room.
    let gap = usize::from(!full && content + FIXED_ROWS + 2 <= gh - 2);
    let height = if full {
        gh
    } else {
        (content + FIXED_ROWS + 2 * gap).min(gh - 2)
    };
    Layout {
        area: Area {
            x0: (gw - width) / 2,
            y0: (gh - height) / 2,
            width,
            height,
            gap,
            two,
        },
        label_w,
        pair,
        more,
    }
}

/// Tab titles along row `y` from `x`, left of `end`, tab `shown`
/// highlighted when `open`, as it isn't while a search spans every tab.
/// When they don't all fit, those before it are left out as far as it
/// takes for it to show. Returns each title's columns, empty for one left
/// out, for taps.
pub(super) fn tab_strip(
    grid: &mut Grid,
    theme: &Theme,
    (x, y, end): (usize, usize, usize),
    (titles, (shown, open)): (&[&str], (usize, bool)),
) -> Vec<Range<usize>> {
    let width = |t: &[&str]| t.iter().map(|t| t.width() + 2).sum::<usize>();
    let room = end.saturating_sub(x);
    let first = (0..=shown)
        .find(|&s| width(&titles[s..=shown]) <= room)
        .unwrap_or(shown);
    let mut at = x;
    let mut ranges = vec![x..x; first];
    for (i, name) in titles.iter().enumerate().skip(first) {
        let style = if open && i == shown {
            let mut s = theme.selected(Style::PLAIN, true);
            s.attributes |= Attributes::BOLD;
            s
        } else {
            Style::PLAIN
        };
        let start = at;
        at = grid.put_str(at, y, end, &format!(" {name} "), style);
        ranges.push(start..at);
    }
    ranges
}

/// A box over the screen at `x0`, `y0`, `width` by `height`: cleared, a
/// border with `title` at its top left and `esc close` at its top right.
pub(super) fn frame(
    grid: &mut Grid,
    (x0, y0, width, height): (usize, usize, usize, usize),
    title: &str,
    theme: &Theme,
) {
    border(grid, (x0, y0, width, height), theme);
    let end = x0 + width;
    grid.put_str(x0 + 2, y0, end - 1, title, theme.style(Role::FileHeader));
    let close = end.saturating_sub(CLOSE.width() + 2);
    grid.put_str(close, y0, end - 1, CLOSE, theme.style(Role::Dim));
}

/// Clears the box and draws its border.
pub(super) fn border(
    grid: &mut Grid,
    (x0, y0, width, height): (usize, usize, usize, usize),
    theme: &Theme,
) {
    let end = x0 + width;
    let border = theme.style(Role::Dim);
    for y in y0..y0 + height {
        grid.fill(x0, y, end, Style::PLAIN);
        let edge = y == y0 || y + 1 == y0 + height;
        let (left, right) = match (y == y0, edge) {
            (true, _) => ("┌", "┐"),
            (false, true) => ("└", "┘"),
            (false, false) => ("│", "│"),
        };
        grid.put_str(x0, y, end, left, border);
        if edge {
            for x in x0 + 1..end - 1 {
                grid.put_str(x, y, end, "─", border);
            }
        }
        grid.put_str(end - 1, y, end, right, border);
    }
}

/// A scrollbar down column `x` from row `top`, `rows` tall, for a list of
/// `len` rows scrolled by `scroll`: a thumb as tall as the share shown,
/// where the view is. Nothing when the list fits.
pub(super) fn scrollbar(
    grid: &mut Grid,
    (x, top, rows): (usize, usize, usize),
    (len, scroll): (usize, usize),
    style: Style,
) {
    if len <= rows || rows == 0 {
        return;
    }
    let thumb = (rows * rows / len).max(1);
    let at = (rows - thumb) * scroll / (len - rows).max(1);
    for i in 0..rows {
        let glyph = if (at..at + thumb).contains(&i) {
            "┃"
        } else {
            "│"
        };
        grid.put_str(x, top + i, x + 1, glyph, style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn shown(help: &mut Help, keys: &Keys, width: usize, height: usize) -> String {
        let mut grid = Grid::new(width, height);
        help.draw(&mut grid, &Theme::default_theme(), keys);
        (0..height)
            .map(|y| grid.row_text(y).trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(help: &mut Help, keys: &Keys, typed: &str) {
        for c in typed.chars() {
            assert!(help.key(keys, Key::Char(c), Mods::NONE), "{c}");
        }
    }

    /// Two columns of key and label, the first key and one alias, the
    /// remapped and unbound keys as in effect. At 200×60 every tab fits
    /// without scrolling, and the active tab is highlighted.
    #[test]
    fn a_wide_screen_gets_two_columns_of_the_keys_in_effect() {
        let mut keys = Keys::default();
        keys.bind("J=scroll_down").expect("valid");
        keys.bind("n=unbind").expect("valid");
        let mut help = Help::default();
        insta::assert_snapshot!(shown(&mut help, &keys, 100, 20));
        let text = shown(&mut help, &keys, 100, 20);
        assert!(text.contains("j J "), "a letter alias over down and ctrl+e");
        assert!(text.contains("} "), "the remaining key of next_change");
        assert!(!text.contains("unbound") && !text.contains("tree:"));

        let theme = Theme::default_theme();
        for tab in 0..TABS.len() {
            let mut help = Help::default();
            help.switch(tab);
            let mut grid = Grid::new(200, 60);
            help.draw(&mut grid, &theme, &keys);
            let text: String = (0..60).map(|y| grid.row_text(y) + "\n").collect();
            let name = TABS[tab].title();
            assert!(!text.contains('┃'), "{name} scrolls:\n{text}");
            let (y, titles) = &help.titles;
            let active = grid.row(*y)[titles[tab].start + 1].style();
            assert_ne!(active, Style::PLAIN, "{name}");
            let other = grid.row(*y)[titles[(tab + 1) % TABS.len()].start + 1].style();
            assert_eq!(other, Style::PLAIN);
        }
    }

    /// The Files tab puts the tree's keys in their own group, without the
    /// `tree:` prefix of `keybind` lines.
    #[test]
    fn the_files_tab_has_the_tree_keys_in_a_group() {
        let keys = Keys::default();
        let mut help = Help::default();
        press(&mut help, &keys, "2");
        insta::assert_snapshot!(shown(&mut help, &keys, 100, 20));
    }

    /// The Review tab titles its groups, Comments, Agents and Review, and
    /// lists a thread's keys under a title of their own; the Git tab lists
    /// a conflict's: `a` asks on a thread, takes the base in a conflict.
    #[test]
    fn the_review_tab_has_a_thread_group_and_git_a_conflict_group() {
        let keys = Keys::default();
        let mut help = Help::default();
        press(&mut help, &keys, "6");
        let text = shown(&mut help, &keys, 200, 60);
        insta::assert_snapshot!(shown(&mut help, &keys, 100, 30));
        let line = |text: &str, wanted: &str| {
            text.lines()
                .position(|l| l.contains(wanted))
                .unwrap_or_else(|| panic!("{wanted}:\n{text}"))
        };
        let thread = line(&text, "on a thread");
        assert!(thread < line(&text, "apply suggestion"), "{text}");
        assert!(
            line(&text, "Agents") < line(&text, "post the review"),
            "{text}"
        );
        press(&mut help, &keys, "4");
        let text = shown(&mut help, &keys, 200, 60);
        assert!(
            line(&text, "in a conflict") < line(&text, "take base"),
            "{text}"
        );
        assert!(line(&text, "Stage, discard & resolve") < line(&text, "stage lines"));
    }

    /// The View tab titles its groups, Layout and Diff, and two columns
    /// split between them rather than inside one.
    #[test]
    fn the_view_tab_splits_its_columns_between_groups() {
        let keys = Keys::default();
        let mut help = Help::default();
        press(&mut help, &keys, "3");
        let text = shown(&mut help, &keys, 100, 20);
        insta::assert_snapshot!(text);
        let first = text.lines().find(|l| l.contains("Layout"));
        assert!(first.is_some_and(|l| l.contains("Diff")), "{text}");
    }

    /// When the tabs don't all fit, the strip starts late enough to show
    /// the one open.
    #[test]
    fn the_open_tab_always_shows() {
        let keys = Keys::default();
        let mut help = Help::default();
        press(&mut help, &keys, "7");
        let text = shown(&mut help, &keys, 40, 15);
        assert!(
            text.lines().nth(1).is_some_and(|l| l.contains(" General ")),
            "{text}"
        );
    }

    /// Moving a thread to this branch is a key of its own, listed as any
    /// other, not one that a focused thread takes over.
    #[test]
    fn moving_a_thread_here_is_listed_with_its_key() {
        let keys = Keys::default();
        let mut help = Help::default();
        press(&mut help, &keys, "/move thread");
        let text = shown(&mut help, &keys, 100, 20);
        let row = text.lines().find(|l| l.contains("move thread here"));
        assert!(row.is_some_and(|r| r.contains("V ")), "{text}");
    }

    #[test]
    fn a_narrow_screen_gets_one_column() {
        let keys = Keys::default();
        let mut help = Help::default();
        let text = shown(&mut help, &keys, 50, 15);
        insta::assert_snapshot!(text);
        assert!(text.contains('┃'), "scrolls");
        for width in [40, 50, 69] {
            for line in shown(&mut help, &keys, width, 15).lines() {
                assert!(line.width() <= width, "{width}: {line}");
            }
        }
        press(&mut help, &keys, "G");
        assert!(shown(&mut help, &keys, 50, 15).contains("scroll to line start"));
    }

    #[test]
    fn slash_searches_every_tab_and_esc_clears_then_closes() {
        let keys = Keys::default();
        let mut help = Help::default();
        press(&mut help, &keys, "/file");
        insta::assert_snapshot!(shown(&mut help, &keys, 100, 20));
        for width in [40, 50] {
            let text = shown(&mut help, &keys, width, 15);
            let row = text.lines().find(|l| l.contains("next file"));
            assert!(
                row.is_some_and(|r| r.trim_end_matches(['│', ' ']).ends_with("Files")),
                "{width}:\n{text}"
            );
        }
        assert!(help.key(&keys, Key::Enter, Mods::NONE));
        assert!(help.key(&keys, Key::Esc, Mods::NONE), "clears");
        assert_eq!(help.filter, "");
        assert!(!help.key(&keys, Key::Esc, Mods::NONE), "closes");
    }

    /// Below the list: where the other settings and keys are, with the
    /// leader key in effect, shortened to fit a phone.
    #[test]
    fn the_footer_points_to_the_panel_and_the_docs() {
        let footer = |keys: &Keys, width| {
            let text = shown(&mut Help::default(), keys, width, 20);
            let lines: Vec<&str> = text.lines().collect();
            let at = lines
                .iter()
                .position(|l| l.contains("tab section"))
                .expect("the hints");
            let inside = |l: &str| l.trim_matches(['│', ' ']).to_owned();
            format!("{}\n{}", inside(lines[at - 1]), inside(lines[at]))
        };
        let mut keys = Keys::default();
        insta::assert_snapshot!(footer(&keys, 100), @r"
        more settings: space · all keys: docs/keys.md
        tab section · / search · esc close
        ");
        keys.bind(",=leader").expect("valid");
        keys.bind("space=unbind").expect("valid");
        insta::assert_snapshot!(footer(&keys, 40), @r"
        settings: , · docs/keys.md
        tab section · / search · esc close
        ");
    }

    /// `tab`, the arrows, `h`/`l` and `1`–`7` switch tabs, round past
    /// either end; a tap on a title opens its tab; other keys close.
    #[test]
    fn keys_and_taps_switch_tabs() {
        let keys = Keys::default();
        let mut help = Help::default();
        let key = |help: &mut Help, key| assert!(help.key(&keys, key, Mods::NONE));
        key(&mut help, Key::Tab);
        assert_eq!(help.tab, 1);
        key(&mut help, Key::Right);
        key(&mut help, Key::Char('l'));
        assert_eq!(help.tab, 3);
        key(&mut help, Key::Left);
        key(&mut help, Key::Char('h'));
        key(&mut help, Key::BackTab);
        key(&mut help, Key::BackTab);
        assert_eq!(help.tab, 6);
        key(&mut help, Key::Char('3'));
        assert_eq!(help.tab, 2);
        shown(&mut help, &keys, 100, 20);
        let (y, titles) = help.titles.clone();
        help.tap(titles[3].start, y);
        assert_eq!(help.tab, 3);
        help.tap(titles[0].start, y + 1);
        assert_eq!(help.tab, 3, "off the titles");
        press(&mut help, &keys, "/wr");
        help.tap(titles[1].start, y);
        assert_eq!(
            (help.tab, help.filter.as_str(), help.editing),
            (1, "", false)
        );
        assert!(!help.key(&keys, Key::Char('q'), Mods::NONE));
    }
}

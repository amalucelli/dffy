//! The command panel the leader key (`space`) opens: the viewer's settings
//! and actions, one per row with its state and its key, in tabs.
//!
//! It opens as a quick popup, sized to its rows at the bottom right so the
//! diff stays in sight, in three titled groups: Here's rows under what
//! they act on (`On this thread`), then [`QUICK`]'s View and Go rows. Those
//! two own their rows, so Here's group leaves out what they list, and they
//! leave out a row whose key a Here row takes. A screen too short for all
//! three drops View. Every row shows one key, the one that does the same
//! outside the panel when there's one, else the panel's; both run it.
//! It opens with no row highlighted, and `space` expands it into the full
//! panel, on Here; once a row is highlighted, run by its key, moved to or
//! tapped, `space` and `enter` run it again and the popup stays. `tab`
//! expands it whenever, `/` into the full panel's search; a setting's key
//! that the popup doesn't list runs it and expands onto its tab, as it
//! shows its tab in the full panel.
//!
//! The full panel opens on Here, the few actions that apply to what
//! you're on; the other tabs hold the leader's table by kind, a tab of
//! several groups titling each, and leave out what can't apply. `tab` and
//! `shift+tab` switch tabs. A row's key runs it straight away from any
//! tab, hidden or not. The other tabs' keys are the leader's table's, each
//! with the key that does the same outside the panel, dimmed, when that's
//! another. Here's rows show one key as the popup's do, and the key from
//! outside runs it ahead of the leader's entry on the same letter: on a
//! thread `r` replies, where the View tab's `r` is the rendered view, and
//! `f` fixes with an agent, where the Files tab's is the next file.
//! `j`/`k` move a cursor and `enter` runs the row under it. Toggles keep
//! the panel open and flash their new state; actions that go somewhere
//! (the picker, the next file) close it. Settings with a few values show
//! `‹ value ›`: `h`/`l`, the arrows or a tap on either mark step back or
//! forward; on other rows `h`/`l` do nothing, so no entry can have them as
//! its key.
//! The theme row opens a list of every theme instead, which tries each one
//! on the screen behind it as the cursor moves. `/` searches every tab by
//! label or key. It takes the `?` overlay's box, tabs and footer, a row
//! its keys first as there: a tab goes in two columns only when the box
//! is too short for it in one and wide enough for two; a narrow screen
//! gets the whole screen and one column.

use std::ops::Range;
use std::time::{Duration, Instant};

use dffy_core::view::Attributes;
use unicode_width::UnicodeWidthStr;

use super::{fuzzy, help};
use crate::keys::{Action, Group, Keys, Tab};
use crate::term::{Grid, Key, Mods};
use crate::theme::{Role, Style, Theme};

const TITLE: &str = " commands ";
/// The footer's first row, the longest that fits; the second is the `?`
/// overlay's.
const FOOTERS: [&str; 3] = [
    "key or enter runs · j/k move · h/l step a value",
    "key or enter runs · j/k move",
    "key runs",
];
const THEMES_TITLE: &str = " themes ";
const THEMES_FOOTER: &str = "type to filter · ↑/↓ try · enter keep · esc back";
/// How long a toggled row's new state stands out.
pub(super) const FLASH: Duration = Duration::from_millis(600);
/// The panel's tabs, in order; it opens on the first.
const TABS: [Place; 6] = [
    Place::Here,
    Place::Tab(Tab::View),
    Place::Tab(Tab::Files),
    Place::Tab(Tab::Review),
    Place::Tab(Tab::Git),
    Place::Tab(Tab::Copy),
];
const COLUMN_GAP: usize = 4;
/// Between a row's keys and its label.
const GAP: usize = 2;
/// The full panel's columns besides its rows': the borders, the padding
/// and the scrollbar.
const CHROME: usize = 5;
/// The quick popup's groups after Here's, by title, each row as far as
/// it applies and no Here row has its key.
const QUICK: [(&str, &[Action]); 2] = [
    (
        "View",
        &[
            Action::CycleLayout,
            Action::ToggleWrap,
            Action::ToggleSyntax,
            Action::ToggleFull,
            Action::ToggleAllFiles,
        ],
    ),
    (
        "Go",
        &[
            Action::OpenPicker,
            Action::JumpBack,
            Action::JumpForward,
            Action::Comments,
            Action::ToggleWatch,
            Action::SinceCheckpoint,
            Action::OpenTerminal,
        ],
    ),
];
/// The quick popup's footer, the longest that fits, with no row
/// highlighted, then with one.
const QUICK_FOOTERS: [[&str; 3]; 2] = [
    [
        "space more · ? keys · esc close",
        "space more · esc close",
        "space more",
    ],
    [
        "space toggles · tab more · esc close",
        "space toggles · tab more",
        "space toggles",
    ],
];
/// The quick popup's blank columns between its border and its rows.
const PAD: usize = 2;

/// Where an entry is listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Place {
    /// What applies to what you're on.
    Here,
    Tab(Tab),
    /// Not where it can't apply; its key still runs it, which says why.
    Hidden,
}

impl Place {
    fn title(self) -> &'static str {
        match self {
            Self::Here => "Here",
            Self::Tab(tab) => tab.title(),
            Self::Hidden => "",
        }
    }
}

/// A row of the panel as the viewer describes it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    pub action: Action,
    /// The key that runs it from the panel; empty when the leader's table
    /// has none for it.
    pub key: String,
    /// A toggle's state; `None` for actions and multi-value settings.
    pub on: Option<bool>,
    pub label: String,
    /// A multi-value setting's value (`auto (split here)`).
    pub value: Option<String>,
    /// Every value it can step to, which the panel is sized for, so the
    /// box stays put as a value steps; empty for other rows.
    pub values: Vec<String>,
    /// The key that does the same outside the panel, when it's another;
    /// on Here it runs the row in the panel too.
    pub direct: Option<String>,
    /// Running it keeps the panel open (a setting) or closes it.
    pub stays: bool,
    pub place: Place,
}

impl Entry {
    /// The one key a Here or quick popup row shows: the one from outside
    /// the panel when there's one, else the panel's.
    fn plain_key(&self) -> &str {
        self.direct.as_deref().unwrap_or(&self.key)
    }

    /// Whether `self` and `other` share a key, panel's or plain.
    fn shares_key(&self, other: &Self) -> bool {
        fn keys(e: &Entry) -> impl Iterator<Item = &str> {
            std::iter::once(e.key.as_str())
                .chain(e.direct.as_deref())
                .filter(|k| !k.is_empty())
        }
        keys(self).any(|k| keys(other).any(|o| o == k))
    }

    /// The columns its label takes with a toggle's `[x] ` before it and a
    /// value's ` ‹ value ›` after, at the widest value it can take.
    fn label_width(&self) -> usize {
        let state = if self.on.is_some() { 4 } else { 0 };
        let value = (self.value.iter().chain(&self.values))
            .map(|v| v.width() + 5)
            .max()
            .unwrap_or(0);
        state + self.label.width() + value
    }
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
    /// Built when the panel opens, and again after a row runs.
    entries: Vec<Entry>,
    shown: Shown,
    /// The quick popup's last draw found the screen too short for its
    /// View group, which it leaves out until the next draw says otherwise.
    short: bool,
    /// The title of the quick popup's Here group: what Here acts on
    /// (`On this thread`).
    title: String,
    /// Of [`TABS`].
    tab: usize,
    /// The entry under the cursor, among those the filter keeps.
    cursor: usize,
    scroll: usize,
    filter: String,
    editing: bool,
    /// Rows the list showed last, for paging and taps.
    page: usize,
    /// Where the list's first row was drawn, for taps.
    top: usize,
    /// The screen columns of each column of rows drawn, and its lines.
    columns: Vec<(Range<usize>, Range<usize>)>,
    /// The screen row the tab titles were drawn on last, and the columns of
    /// each, for taps.
    titles: (usize, Vec<Range<usize>>),
    /// The action whose row flashes, and since when.
    flash: Option<(Action, Instant)>,
    /// The `‹` and `›` of each value drawn: screen row, then their columns.
    arrows: Vec<(usize, usize, usize)>,
    /// The theme list, while it's open over the panel.
    themes: Option<ThemeList>,
}

/// What the panel shows.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Shown {
    #[default]
    Full,
    /// The quick popup with no row highlighted: `space` expands it.
    Quick,
    /// The quick popup with the row at the cursor highlighted, moved to,
    /// run by its key or tapped: `space` runs it.
    Picked,
}

/// A row of the list as drawn: a group's title, or an entry by its index.
enum Line {
    Title(&'static str),
    Entry(usize),
    Blank,
    /// The quick popup's Here group's title, [`Commands::title`].
    Here,
}

impl Commands {
    /// The quick popup, its Here group titled `title`.
    pub(super) fn new(title: String, entries: Vec<Entry>) -> Self {
        Self {
            entries,
            shown: Shown::Quick,
            title,
            ..Self::default()
        }
    }

    /// The title and entries as things stand now, after a row ran.
    pub(super) fn update(&mut self, title: String, entries: Vec<Entry>) {
        self.title = title;
        self.entries = entries;
    }

    /// Whether running `action` keeps the panel open.
    pub(super) fn stays(&self, action: Action) -> bool {
        self.entries.iter().any(|e| e.action == action && e.stays)
    }

    /// The entries the filter keeps with the label's matched characters:
    /// the tab's, by group, then as the leader's table lists them; with a
    /// filter, the best matches of label or key across every tab, an
    /// action in Here and its own tab once.
    fn kept(&self) -> Vec<(&Entry, Vec<usize>)> {
        if self.quick() {
            return self.in_quick();
        }
        if self.filter.is_empty() {
            return self.in_tab(self.tab);
        }
        let entries = &self.entries;
        let elsewhere = |e: &Entry| {
            e.place == Place::Here
                && (entries.iter())
                    .any(|o| o.action == e.action && matches!(o.place, Place::Tab(_)))
        };
        let mut scored: Vec<(i64, &Entry, Vec<usize>)> = entries
            .iter()
            .filter(|e| e.place != Place::Hidden && !elsewhere(e))
            .filter_map(|e| {
                let label = crate::fuzzy::score_close(&self.filter, &e.label);
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

    /// Tab `tab`'s entries: by group, then as the leader's table lists
    /// them; Here's as the viewer gave them.
    fn in_tab(&self, tab: usize) -> Vec<(&Entry, Vec<usize>)> {
        let place = TABS[tab];
        let mut kept: Vec<(&Entry, Vec<usize>)> = (self.entries.iter())
            .filter(|e| e.place == place)
            .map(|e| (e, Vec::new()))
            .collect();
        if place != Place::Here {
            kept.sort_by_key(|(e, _)| e.action.group());
        }
        kept
    }

    /// The quick popup's entries: [`Commands::quick_groups`]' in order.
    fn in_quick(&self) -> Vec<(&Entry, Vec<usize>)> {
        (self.quick_groups().into_iter())
            .flat_map(|(_, group)| group)
            .map(|e| (e, Vec::new()))
            .collect()
    }

    /// The quick popup's groups, `None` for Here's title: Here's entries
    /// but the leader's own (all keys, which the footer points to) and
    /// those [`QUICK`]'s groups list, then each of those with its entries
    /// that apply, from their tabs, unless a Here row has a key of theirs,
    /// so every key shown runs what it shows. View is left out on a short
    /// screen.
    fn quick_groups(&self) -> Vec<(Option<&'static str>, Vec<&Entry>)> {
        let quick: Vec<(&'static str, Vec<&Entry>)> = QUICK
            .iter()
            .map(|&(title, actions)| {
                let entries = (actions.iter()).filter_map(|&action| {
                    (self.entries.iter())
                        .find(|e| e.action == action && matches!(e.place, Place::Tab(_)))
                });
                (title, entries.collect())
            })
            .collect();
        let listed = |e: &Entry| {
            quick
                .iter()
                .flat_map(|(_, g)| g)
                .any(|q| q.action == e.action)
        };
        let here: Vec<&Entry> = (self.entries.iter())
            .filter(|e| e.place == Place::Here && e.action.group() != Group::General && !listed(e))
            .collect();
        let quick: Vec<_> = (quick.into_iter())
            .filter(|&(title, _)| !(self.short && title == QUICK[0].0))
            .map(|(title, group)| {
                let kept = group
                    .into_iter()
                    .filter(|e| !here.iter().any(|h| h.shares_key(e)));
                (Some(title), kept.collect())
            })
            .collect();
        std::iter::once((None, here)).chain(quick).collect()
    }

    /// The rows of tab `tab`, or without one, a search's matches alone. A
    /// tab's groups go under their titles when it has more than one;
    /// Here's rows keep their order, the leader's own for it after a blank.
    fn lines(kept: &[(&Entry, Vec<usize>)], tab: Option<usize>) -> Vec<Line> {
        let Some(tab) = tab else {
            return (0..kept.len()).map(Line::Entry).collect();
        };
        let here = TABS[tab] == Place::Here;
        let group = |e: &Entry| -> Option<Group> {
            let group = e.action.group();
            (!here || group == Group::General).then_some(group)
        };
        let titled = !here && kept.windows(2).any(|w| group(w[0].0) != group(w[1].0));
        let mut out = Vec::new();
        let mut last = None;
        for (i, (e, _)) in kept.iter().enumerate() {
            let starts = last != Some(group(e));
            if last.is_some() && starts {
                out.push(Line::Blank);
            }
            if titled && starts {
                out.push(Line::Title(e.action.group().title()));
            }
            last = Some(group(e));
            out.push(Line::Entry(i));
        }
        out
    }

    /// Where `lines` splits into a left and a right column: at the blank
    /// between groups nearest half way, else half way.
    fn split(lines: &[Line]) -> usize {
        let half = lines.len().div_ceil(2);
        (0..lines.len())
            .filter(|&i| matches!(lines[i], Line::Blank))
            .min_by_key(|&i| i.abs_diff(half))
            .unwrap_or(half)
    }

    /// The lines of each column, as ranges of `lines`: two when `two`. The
    /// blank row the columns split at shows in neither.
    fn column_ranges(lines: &[Line], two: bool) -> Vec<Range<usize>> {
        let at = if two { Self::split(lines) } else { lines.len() };
        let right = at + usize::from(matches!(lines.get(at), Some(Line::Blank)));
        std::iter::once(0..at)
            .chain(two.then_some(right..lines.len()))
            .collect()
    }

    /// The lines of what's shown: the quick popup's, the tab's, or a
    /// search's matches.
    fn shown_lines(&self, kept: &[(&Entry, Vec<usize>)]) -> Vec<Line> {
        if self.quick() {
            let mut out = Vec::new();
            let mut i = 0;
            for (title, group) in self.quick_groups() {
                if group.is_empty() {
                    continue;
                }
                if !out.is_empty() {
                    out.push(Line::Blank);
                }
                out.push(title.map_or(Line::Here, Line::Title));
                out.extend((i..i + group.len()).map(Line::Entry));
                i += group.len();
            }
            return out;
        }
        Self::lines(kept, self.filter.is_empty().then_some(self.tab))
    }

    /// Whether `e`, shown, shows only [`Entry::plain_key`], which runs it
    /// too: the quick popup's rows, and Here's on Here while no search
    /// hides it.
    fn direct_runs(&self, e: &Entry) -> bool {
        self.quick()
            || (e.place == Place::Here && TABS[self.tab] == Place::Here && self.filter.is_empty())
    }

    /// Puts the cursor on row `i`, which the quick popup highlights.
    fn pick(&mut self, i: usize) {
        self.cursor = i;
        if self.shown == Shown::Quick {
            self.shown = Shown::Picked;
        }
    }

    /// Shows tab `tab` of the full panel, wrapping round, with the search
    /// cleared.
    fn switch(&mut self, tab: usize) {
        self.shown = Shown::Full;
        self.tab = tab % TABS.len();
        self.filter.clear();
        self.editing = false;
        self.cursor = 0;
        self.scroll = 0;
    }

    #[cfg(test)]
    pub(super) fn cursor(&self) -> usize {
        self.cursor
    }

    /// The quick popup shows, not the full panel.
    pub(super) fn quick(&self) -> bool {
        self.shown != Shown::Full
    }

    #[cfg(test)]
    pub(super) fn tab(&self) -> &'static str {
        TABS[self.tab].title()
    }

    /// The action of row `i` among those the filter keeps.
    #[cfg(test)]
    pub(super) fn entry(&self, i: usize) -> Option<Action> {
        self.kept().get(i).map(|(e, _)| e.action)
    }

    /// How many rows the filter keeps.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.kept().len()
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

    /// What `key` does in the quick popup, where that's its own; `None` for
    /// the keys it shares with the full panel. `space` expands it until a
    /// row is highlighted and then runs that row, a value stepping forward,
    /// the popup staying open on its new state; `tab` and `/` expand it
    /// whenever; the first move highlights the first row, or up the last.
    fn quick_key(&mut self, key: Key) -> Option<Outcome> {
        let picked = match self.shown {
            Shown::Full => return None,
            Shown::Quick => false,
            Shown::Picked => true,
        };
        let (len, at) = {
            let kept = self.kept();
            (kept.len(), kept.get(self.cursor).map(|(e, _)| e.action))
        };
        match key {
            Key::Char(' ') | Key::Enter if picked => {
                Some(at.map_or(Outcome::Stay, |action| Outcome::Run {
                    action,
                    back: false,
                }))
            }
            Key::Enter => Some(Outcome::Stay),
            Key::Char(' ' | '/') | Key::Tab | Key::BackTab => {
                self.switch(0);
                self.editing = matches!(key, Key::Char('/'));
                Some(Outcome::Stay)
            }
            Key::Down | Key::Char('j') | Key::PageDown if !picked && len > 0 => {
                self.pick(0);
                Some(Outcome::Stay)
            }
            Key::Up | Key::Char('k') | Key::PageUp if !picked && len > 0 => {
                self.pick(len - 1);
                Some(Outcome::Stay)
            }
            _ => None,
        }
    }

    pub(super) fn key(&mut self, key: Key, mods: Mods) -> Outcome {
        if let Some(list) = &mut self.themes {
            let outcome = list.key(key, mods);
            if matches!(outcome, Outcome::Theme { keep: true, .. }) || list.closing {
                self.themes = None;
            }
            return outcome;
        }
        if let Some(outcome) = self.quick_key(key) {
            return outcome;
        }
        let kept: Vec<&Entry> = self.kept().into_iter().map(|(e, _)| e).collect();
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
        // The quick popup acts on no row until one is highlighted.
        let on_value =
            self.shown != Shown::Quick && kept.get(self.cursor).is_some_and(|e| e.value.is_some());
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
            Key::Tab => self.switch(self.tab + 1),
            Key::BackTab => self.switch(self.tab + TABS.len() - 1),
            Key::Enter | Key::Char(' ') => {
                return kept
                    .get(self.cursor)
                    .map_or(Outcome::Stay, |e| Self::run(e, false));
            }
            // Kept from before the `‹ ›` marks, as a hidden alias of `h`;
            // the quick popup shows backspace as Back's key.
            Key::Backspace if on_value && self.shown == Shown::Full => return step(true),
            // Past either end, the cursor goes round.
            Key::Down | Key::Char('j') if len > 0 => self.cursor = (self.cursor + 1) % len,
            Key::Up | Key::Char('k') if len > 0 => self.cursor = (self.cursor + len - 1) % len,
            Key::PageDown => self.cursor = (self.cursor + page).min(len.saturating_sub(1)),
            Key::PageUp => self.cursor = self.cursor.saturating_sub(page),
            _ => {
                let name = crate::keys::Chord { key, mods }.name();
                // A row's own key steps it forward, theme included; on
                // Here, so does the key it shows from outside the panel.
                let direct = |e: &&Entry| e.direct.as_ref() == Some(&name) && self.direct_runs(e);
                let shown = (kept.iter().position(|e| e.key == name))
                    .or_else(|| kept.iter().position(direct));
                if let Some(i) = shown {
                    let action = kept[i].action;
                    self.pick(i);
                    return Outcome::Run {
                        action,
                        back: false,
                    };
                }
                let found = (self.entries.iter())
                    .filter(|e| e.key == name)
                    .min_by_key(|e| e.place == Place::Here);
                let Some(&Entry {
                    action,
                    stays,
                    place,
                    ..
                }) = found
                else {
                    return Outcome::Unknown;
                };
                // A setting shows the tab it's in, the cursor on it, so
                // its new state shows and `h`/`l` step it.
                if let (true, Some(tab)) = (stays, TABS.iter().position(|&t| t == place)) {
                    self.switch(tab);
                    self.cursor = (self.kept().iter())
                        .position(|(e, _)| e.action == action)
                        .unwrap_or(0);
                }
                return Outcome::Run {
                    action,
                    back: false,
                };
            }
        }
        Outcome::Stay
    }

    /// The wheel moves the cursor.
    pub(super) fn scroll(&mut self, rows: isize) -> Outcome {
        if let Some(list) = &mut self.themes {
            return list.scroll(rows);
        }
        let len = self.kept().len();
        self.pick(
            self.cursor
                .saturating_add_signed(rows)
                .min(len.saturating_sub(1)),
        );
        Outcome::Stay
    }

    /// A tap at screen cell `(x, y)`: on a tab's title, that tab; on a
    /// value's `‹` or `›` it steps the value; elsewhere on a row, the entry
    /// runs; outside the list, the panel closes.
    pub(super) fn tap(&mut self, (x, y): (usize, usize)) -> Outcome {
        if let Some(list) = &mut self.themes {
            let outcome = list.tap(y);
            if matches!(outcome, Outcome::Theme { keep: true, .. }) || list.closing {
                self.themes = None;
            }
            return outcome;
        }
        let (titles_y, titles) = &self.titles;
        if !self.quick() && y == *titles_y {
            if let Some(tab) = titles.iter().position(|r| r.contains(&x)) {
                self.switch(tab);
            }
            return Outcome::Stay;
        }
        let arrow = (self.arrows.iter())
            .find(|&&(row, left, right)| row == y && (x == left || x == right))
            .map(|&(_, left, _)| x == left);
        let Some(row) = y.checked_sub(self.top).filter(|&r| r < self.page) else {
            return Outcome::Close;
        };
        let Some(at) = (self.columns.iter())
            .find(|(columns, _)| columns.contains(&x))
            .map(|(_, lines)| (lines.start + self.scroll + row, lines))
            .filter(|(at, lines)| lines.contains(at))
            .map(|(at, _)| at)
        else {
            return Outcome::Stay;
        };
        let kept = self.kept();
        let lines = self.shown_lines(&kept);
        let Some(&Line::Entry(i)) = lines.get(at) else {
            return Outcome::Stay;
        };
        let outcome = match arrow {
            Some(back) => Outcome::Run {
                action: kept[i].0.action,
                back,
            },
            None => Self::run(kept[i].0, false),
        };
        self.pick(i);
        outcome
    }

    /// The widest panel key, and the widest with the key from outside the
    /// panel after it: the column the rows' labels start after.
    fn key_widths(&self) -> (usize, usize) {
        let shown = || self.entries.iter().filter(|e| e.place != Place::Hidden);
        let panel = shown().map(|e| e.key.width()).max().unwrap_or(0);
        let both = shown()
            .map(|e| e.key.width() + e.direct.as_ref().map_or(0, |d| d.width() + 1))
            .max()
            .unwrap_or(0);
        (panel, both)
    }

    /// Draws the panel in the box the `?` overlay takes for `keys`, with
    /// its tabs and footer: a tab's rows in one column while they fit the
    /// box's height, else in two where the box is as wide as the
    /// overlay's two.
    #[expect(
        clippy::too_many_lines,
        reason = "the panel in order: its box and tabs, each column's rows, the footer"
    )]
    pub(super) fn draw(&mut self, grid: &mut Grid, theme: &Theme, keys: &Keys) {
        let (gw, gh) = (grid.width(), grid.height());
        if gw < 12 || gh < 6 {
            return;
        }
        if let Some(list) = &mut self.themes {
            list.draw(grid, theme);
            return;
        }
        if self.quick() {
            self.draw_quick(grid, theme);
            return;
        }
        let mut area = help::area(keys, (gw, gh));
        // The box is the help overlay's, its columns sized by the help's
        // rows; a row of the panel's wider than that, a long value, widens
        // it as far as the screen goes, else the panel takes one column.
        if area.two {
            let (_, both) = self.key_widths();
            let shown = self.entries.iter().filter(|e| e.place != Place::Hidden);
            let row = shown
                .map(|e| both + GAP + e.label_width())
                .max()
                .unwrap_or(0);
            let wanted = 2 * row + COLUMN_GAP + CHROME;
            if wanted > gw.saturating_sub(4) {
                area.two = false;
            } else if wanted > area.width {
                area.width = wanted;
                area.x0 = (gw - wanted) / 2;
            }
        }
        let help::Area {
            x0,
            y0,
            width,
            height,
            ..
        } = area;
        let end = x0 + width;
        let (x, right) = (x0 + 2, end - 3);
        let border = theme.style(Role::Dim);
        help::frame(grid, (x0, y0, width, height), TITLE, theme);
        let searching = self.editing || !self.filter.is_empty();
        let titles = (&TABS.map(Place::title)[..], (self.tab, !searching));
        self.titles = (
            y0 + 1,
            help::tab_strip(grid, theme, (x, y0 + 1, right), titles),
        );

        let kept = self.kept();
        let lines = self.shown_lines(&kept);
        let cursor = self.cursor.min(kept.len().saturating_sub(1));
        let (rows, top) = (area.list_rows(), area.top());
        let two = area.two && !searching && lines.len() > rows;
        let ranges = Self::column_ranges(&lines, two);
        let column_w = if two {
            (right - x).saturating_sub(COLUMN_GAP) / 2
        } else {
            right - x
        };
        let at = (lines.iter())
            .position(|l| matches!(l, Line::Entry(i) if *i == cursor))
            .unwrap_or(0);
        let row = (ranges.iter())
            .find(|r| r.contains(&at))
            .map_or(0, |r| at - r.start);
        let mut scroll = self.scroll;
        if row < scroll {
            scroll = row.saturating_sub(1);
        } else if row >= scroll + rows {
            scroll = row + 1 - rows;
        }
        let len = ranges.iter().map(ExactSizeIterator::len).max().unwrap_or(0);
        let scroll = scroll.min(len.saturating_sub(rows));
        let flashing = (self.flash)
            .filter(|(_, at)| at.elapsed() < FLASH)
            .map(|(a, _)| a);
        let keys_widths = self.key_widths();
        let mut arrows = Vec::new();
        let mut columns = Vec::new();
        for (c, range) in ranges.iter().enumerate() {
            let cx = x + c * (column_w + COLUMN_GAP);
            let column_end = if c + 1 == ranges.len() {
                right
            } else {
                cx + column_w
            };
            columns.push((cx - 1..column_end + 1, range.clone()));
            for (i, line) in range.clone().skip(scroll).take(rows).enumerate() {
                let y = top + i;
                match lines[line] {
                    Line::Title(text) => {
                        let mut bold = theme.style(Role::Plain);
                        bold.attributes |= Attributes::BOLD;
                        grid.put_str(cx, y, column_end, text, bold);
                    }
                    Line::Entry(k) => {
                        let (e, matched) = &kept[k];
                        let row = Row {
                            selected: k == cursor,
                            flashing: flashing == Some(e.action),
                            direct_runs: self.direct_runs(e),
                        };
                        let place = searching.then(|| e.place.title());
                        let at = (cx, y, column_end);
                        if let Some((l, r)) =
                            row.draw(grid, theme, at, (e, matched), (keys_widths, place))
                        {
                            arrows.push((y, l, r));
                        }
                    }
                    Line::Blank | Line::Here => {}
                }
            }
        }
        help::scrollbar(grid, (end - 2, top, rows), (len, scroll), border);
        let hints = if searching {
            help::search_hint(&self.filter, self.editing)
        } else {
            help::HINTS.to_owned()
        };
        let room = right - x;
        let runs = (FOOTERS.iter())
            .find(|f| f.width() <= room)
            .unwrap_or(&FOOTERS[FOOTERS.len() - 1]);
        help::footer(grid, &area, [runs, &hints], theme);
        drop(kept);
        self.cursor = cursor;
        self.scroll = scroll;
        self.page = rows;
        self.top = top;
        self.arrows = arrows;
        self.columns = columns;
    }

    /// Draws the quick popup: its rows one each, under their groups'
    /// titles with a blank row between groups, between [`PAD`] blank
    /// columns and a blank row either side, and its footer. Sized to its
    /// rows, at the bottom right above the screen's last row, which holds
    /// the status bar or the frame's edge; a narrow screen gets it full
    /// width. Too tall for the screen, it leaves out its View group, then
    /// scrolls with the cursor.
    fn draw_quick(&mut self, grid: &mut Grid, theme: &Theme) {
        let (gw, gh) = (grid.width(), grid.height());
        self.short = false;
        // Borders, blank rows above and below the list, and the footer.
        self.short = self.shown_lines(&[]).len() + 5 > gh - 1;
        let kept = self.kept();
        let lines = self.shown_lines(&kept);
        let cursor = self.cursor.min(kept.len().saturating_sub(1));
        let widest = |f: &dyn Fn(&Entry) -> usize| kept.iter().map(|(e, _)| f(e)).max();
        let key = widest(&|e| e.plain_key().width()).unwrap_or(0);
        let label = widest(&Entry::label_width);
        let inner = (key + GAP + label.unwrap_or(0))
            .max(QUICK_FOOTERS[0][0].width())
            .max(QUICK_FOOTERS[1][0].width())
            .max(TITLE.width())
            .max(self.title.width());
        let (x0, width) = if gw < help::FULL_SCREEN_BELOW {
            (0, gw)
        } else {
            let width = (inner + 2 * PAD + 2).min(gw - 1);
            (gw - 1 - width, width)
        };
        let height = (lines.len() + 5).min(gh - 1);
        let y0 = gh - 1 - height;
        let end = x0 + width;
        let (x, right) = (x0 + 1 + PAD, end - 1 - PAD);
        help::border(grid, (x0, y0, width, height), theme);
        grid.put_str(x0 + 2, y0, end - 1, TITLE, theme.style(Role::FileHeader));

        let (top, rows) = (y0 + 2, height.saturating_sub(5));
        let at = (lines.iter())
            .position(|l| matches!(l, Line::Entry(i) if *i == cursor))
            .unwrap_or(0);
        let mut scroll = self.scroll;
        if at < scroll {
            scroll = at.saturating_sub(1);
        } else if at >= scroll + rows {
            scroll = at + 1 - rows;
        }
        let scroll = scroll.min(lines.len().saturating_sub(rows));
        let flashing = (self.flash)
            .filter(|(_, at)| at.elapsed() < FLASH)
            .map(|(a, _)| a);
        let dim = theme.style(Role::Dim);
        let mut bold = theme.style(Role::Plain);
        bold.attributes |= Attributes::BOLD;
        let mut arrows = Vec::new();
        for (i, line) in lines.iter().skip(scroll).take(rows).enumerate() {
            let y = top + i;
            match *line {
                Line::Entry(k) => {
                    let (e, matched) = &kept[k];
                    let row = Row {
                        selected: self.shown == Shown::Picked && k == cursor,
                        flashing: flashing == Some(e.action),
                        direct_runs: self.direct_runs(e),
                    };
                    let widths = ((key, key), None);
                    if let Some((l, r)) = row.draw(grid, theme, (x, y, right), (e, matched), widths)
                    {
                        arrows.push((y, l, r));
                    }
                }
                Line::Title(text) => {
                    grid.put_str(x, y, right, text, bold);
                }
                Line::Here => {
                    grid.put_str(x, y, right, &self.title, bold);
                }
                Line::Blank => {}
            }
        }
        help::scrollbar(grid, (end - 2, top, rows), (lines.len(), scroll), dim);
        let footers = &QUICK_FOOTERS[usize::from(self.shown == Shown::Picked)];
        let footer = (footers.iter())
            .find(|f| f.width() <= right - x)
            .unwrap_or(&footers[footers.len() - 1]);
        grid.put_str(x, y0 + height - 2, right, footer, dim);
        let columns = vec![(x - 1..right + 1, 0..lines.len())];
        drop(kept);
        self.cursor = cursor;
        self.scroll = scroll;
        self.page = rows;
        self.top = top;
        self.arrows = arrows;
        self.columns = columns;
    }
}

struct Row {
    /// Under the cursor: in the theme's selection, the label bold.
    selected: bool,
    /// Just toggled: its state stands out.
    flashing: bool,
    /// Its key from outside the panel runs it here: it shows that one
    /// alone, [`Entry::plain_key`].
    direct_runs: bool,
}

impl Row {
    /// `key direct  [x] Label ‹ value ›  ...  place` between `x` and
    /// `right`, as the `?` overlay writes a row: the keys first, the panel
    /// key `panel` wide and the direct key after it, dimmed, or the plain
    /// key alone where it runs the row; then `both` columns on the label,
    /// its `matched` characters marked, a toggle's state before it and a
    /// value after; `place`, a search's tab name, dimmed at the right where
    /// it fits. Returns the columns of the value's `‹` and `›`, when it has
    /// one.
    fn draw(
        &self,
        grid: &mut Grid,
        theme: &Theme,
        (x, y, right): (usize, usize, usize),
        (e, matched): (&Entry, &[usize]),
        ((panel, both), place): ((usize, usize), Option<&str>),
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
        let dim = mark(theme.style(Role::Dim));
        let hint = mark(theme.style(Role::Hint));
        if self.direct_runs {
            grid.put_str(x, y, right, e.plain_key(), hint);
        } else {
            grid.put_str(x, y, right, &e.key, hint);
            if let Some(direct) = &e.direct {
                grid.put_str(x + panel + 1, y, right, direct, dim);
            }
        }
        let style = if self.flashing {
            theme.style(Role::Hint)
        } else {
            Style::PLAIN
        };
        let bold = |mut s: Style| {
            if self.selected {
                s.attributes |= Attributes::BOLD;
            }
            s
        };
        let mut at = x + both + GAP;
        if let Some(on) = e.on {
            let state = if on { "[x] " } else { "[ ] " };
            at = grid.put_str(at, y, right, state, mark(style));
        }
        let mut written = fuzzy::put_marked(
            grid,
            (at, y, right),
            &e.label,
            matched,
            (
                bold(mark(Style::PLAIN)),
                bold(mark(theme.style(Role::Hint))),
            ),
        );
        let arrows = e.value.as_ref().map(|value| {
            let left = at + e.label.width() + 1;
            let after = grid.put_str(left, y, right, "‹ ", dim);
            // Cut to leave room for the closing mark, never silently.
            let room = right.saturating_sub(after + 2);
            let value = super::tree::fit_right(value, room);
            let after = grid.put_str(after, y, right, &value, mark(style));
            written = grid.put_str(after, y, right, " ›", dim);
            (left, written - 1)
        });
        if let Some(place) = place {
            let at = right.saturating_sub(place.width());
            if at > written + 1 {
                grid.put_str(at, y, right, place, dim);
            }
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
                crate::fuzzy::score_close(&self.filter, name).map(|(s, at)| (s, i, at))
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
        let full = gw < help::FULL_SCREEN_BELOW;
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
                    bold.attributes |= Attributes::BOLD;
                    grid.put_str(x, y, right, text, bold);
                }
                Line::Entry(entry) => {
                    let (index, matched) = &kept[entry];
                    let name = &self.names[*index].0;
                    let selected = entry == self.cursor;
                    let mark = |s: Style| {
                        if selected {
                            let mut s = theme.selected(s, true);
                            s.attributes |= Attributes::BOLD;
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
                Line::Blank | Line::Here => {}
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

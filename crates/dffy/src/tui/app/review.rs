//! Review mode in the viewer: the keys and overlays of the review session
//! [`crate::review::session`] keeps, and what its effects do to the
//! viewer. The post dialog and the review picker are drawn here.

use std::ops::ControlFlow;
use std::time::Instant;

use dffy_core::git::Revisions;
use dffy_core::view::Attributes;
use unicode_width::UnicodeWidthStr;

use super::{Action, App, escape};
use crate::keys::Keys;
use crate::plugin::review::{Listed, ReviewEvent, ReviewLabel, Said, Session};
use crate::review::model;
use crate::review::session::{
    self, Checked, Effect, Form, Mode, Offer, Post, Reviewing, Sending, Shown, plural,
};
use crate::term::{self, Grid, Input, Key, Mods};
use crate::theme::{Role, Style, Theme};
use crate::tui::help;
use crate::tui::textbox::TextBox;

/// The dialog `P` opens to post the review: a summary, the plugin's
/// events, and one line when something won't go.
pub(super) struct Dialog {
    /// The plugin's events, and the one picked.
    events: Vec<ReviewEvent>,
    event: usize,
    /// The session's labels, each with the value picked, by index.
    labels: Vec<(ReviewLabel, usize)>,
    /// The line j/k are on: an event's, then a label's.
    row: usize,
    summary: TextBox,
    /// Keys go to the summary.
    writing: bool,
    /// The local drafts in view go with the review (`l`).
    local: bool,
}

impl Dialog {
    fn new(form: Form) -> Self {
        Self {
            row: form.event,
            summary: TextBox::new(&form.summary),
            events: form.events,
            event: form.event,
            labels: form.labels,
            writing: false,
            local: form.local,
        }
    }
}

/// The post dialog's key going to the drafts that won't post as they are.
pub(super) const GO_TO_UNPOSTED: char = 'v';

/// The key opening the review just posted, while its note shows.
pub(super) const OPEN_POSTED: char = 'o';

/// The reviews offered to open one, as the session grouped them.
pub(super) struct Picker {
    offer: Offer,
    /// The widest tag, kept to a few columns.
    tag_width: usize,
    cursor: usize,
    scroll: usize,
    /// The plugin's plural noun, for the frame.
    title: String,
}

impl Picker {
    fn new(mut offer: Offer, nouns: &str) -> Self {
        for text in offer.tags.iter_mut().chain(&mut offer.groups) {
            *text = escape(text.as_bytes());
        }
        let tag_width = (offer.tags.iter())
            .map(|t| t.width())
            .max()
            .unwrap_or(0)
            .clamp(7, 16);
        Self {
            offer,
            tag_width,
            cursor: 0,
            scroll: 0,
            title: format!(" {nouns} "),
        }
    }
}

impl App {
    /// Carries out what the review session said, in order: the last words
    /// said are the status bar's.
    pub(super) fn review_effects(&mut self, effects: Vec<Effect>) -> Option<Action> {
        let mut action = None;
        for effect in effects {
            match effect {
                Effect::Say(text) => self.message(text),
                Effect::Hush => self.message = None,
                Effect::Fail(text) => self.failed(text),
                Effect::Finished(text) => {
                    self.unseen = Some(term::Status {
                        state: term::State::Done,
                        message: Some(text),
                        ..term::Status::default()
                    });
                }
                Effect::Ask { id, request } => action = Some(Action::Review { id, request }),
                Effect::Show {
                    range,
                    comments,
                    moved,
                } => {
                    action = Some(Action::Retarget {
                        revisions: range.map(|range| self.show_range(range)),
                        comments,
                        moved,
                    });
                }
                Effect::Describe(description) => self.description = description,
                Effect::Changed(paths) => self.placed_again(&paths),
                Effect::Focus { root, note } => {
                    self.follow_thread(&root);
                    self.leave_panel();
                    self.message(self.say(&note));
                }
                Effect::NewThreads(roots) => {
                    let next = self
                        .key_for(false, crate::keys::Action::NextThread)
                        .unwrap_or_default();
                    let note = format!(
                        "{} · {next} goes to them",
                        plural(roots.len(), "new comment")
                    );
                    self.new_threads = roots;
                    self.message(note);
                }
                Effect::Pick(offer) => {
                    self.reviews = Some(Picker::new(offer, self.reviewing.noun(true)));
                }
                Effect::Redraw => self.dirty = true,
            }
        }
        action
    }

    /// Takes a review's range as what's shown: its words in the title, and
    /// the revisions an empty list is worded by.
    fn show_range(&mut self, range: session::Range) -> Revisions {
        self.description = range.description;
        self.doc.page_title = range.title;
        if let Some((selected, _)) = &mut self.selected {
            selected.clone_from(&range.revisions);
        }
        range.revisions
    }

    /// Starts on `session`, whose `prepare` goes on in the background and
    /// says the rest under the returned run id.
    pub(crate) fn start_review(&mut self, plugin: String, session: Session) -> u64 {
        let comments = self.comments.as_mut();
        let (id, effects) = self
            .reviewing
            .start((plugin, session), comments, Instant::now());
        self.review_effects(effects);
        id
    }

    /// Opens the picker on `reviews`, for the review command (`dffy pr`) on a
    /// branch without one.
    pub(crate) fn pick_review(&mut self, plugin: String, reviews: Vec<Listed>) {
        let effects = self.reviewing.pick(plugin, reviews);
        self.review_effects(effects);
    }

    /// The command panel's "Open review…", in the plugin's words.
    pub(super) fn open_review_picker(&mut self) -> Option<Action> {
        if self.piped {
            self.message("a piped diff can't be reviewed");
            return None;
        }
        let effects = self.reviewing.list();
        self.review_effects(effects)
    }

    /// Asks `plugin` whether the branch checked out has a review, for
    /// plain `dffy`; the viewer goes on meanwhile.
    pub(crate) fn detect_review(&mut self, plugin: String) -> Action {
        let (id, request) = self.reviewing.detect(plugin);
        Action::Review { id, request }
    }

    /// A tick: you may have gone idle, a read of the comments held back
    /// and the drafts' plan may be due.
    pub(super) fn review_tick(&mut self) -> Option<Action> {
        let now = Instant::now();
        self.reviewing.went_idle(self.last_input, now);
        let mut effects = self.reviewing.due_reload(self.comments.as_mut(), now);
        effects.extend(self.reviewing.due_plan(self.comments.as_ref(), now));
        self.review_effects(effects)
    }

    /// Whether a post is running: it can't be stopped halfway.
    pub(crate) fn posting_now(&self) -> bool {
        self.reviewing.posting_now()
    }

    /// `p`: fetches the review's head and threads.
    pub(super) fn update_review(&mut self) -> Option<Action> {
        let effects = self.reviewing.update();
        self.review_effects(effects)
    }

    /// `P`: the dialog that posts the review.
    pub(super) fn open_post(&mut self) -> Option<Action> {
        let (form, effects) = self.reviewing.open_post(self.comments.as_ref());
        if let Some(form) = form {
            self.posting = Some(Dialog::new(form));
        }
        self.review_effects(effects)
    }

    /// The drafts' plan, due soon: they changed.
    pub(super) fn plan_soon(&mut self) {
        self.reviewing.plan_soon(Instant::now());
    }

    /// What the dialog says goes, the local drafts in view only with
    /// `local`.
    pub(super) fn sending(&self, local: bool) -> Sending {
        self.reviewing.sending(self.comments.as_ref(), local)
    }

    /// "Since last review": the range from the head your last review was
    /// on to the head, or back to what showed before.
    pub(super) fn toggle_since_review(&mut self) -> Option<Action> {
        let effects = self.reviewing.toggle_since();
        self.review_effects(effects)
    }

    /// "Show whole review": on the review plain `dffy` found, the review's
    /// base against the working tree, or plain `dffy`'s range again.
    pub(super) fn toggle_whole_review(&mut self) -> Option<Action> {
        let effects = self.reviewing.toggle_whole();
        self.review_effects(effects)
    }

    /// The head of your last review and the head, while "Since last review"
    /// shows from it.
    pub(crate) fn reviewed_from(&self) -> Option<(&str, &str)> {
        self.reviewing.reviewed_from()
    }

    /// The head of your last review isn't in the clone, so the range from
    /// it can't be listed: back to the whole review, whose range is
    /// returned.
    pub(crate) fn reviewed_head_missing(&mut self) -> Option<Revisions> {
        let (note, range) = self.reviewing.head_missing()?;
        self.message(note);
        Some(self.show_range(range))
    }

    /// The review was rebased between your last review's head `last` and
    /// `head`: the range from it takes in what the base gained too, which
    /// the note says, while that range still shows.
    pub(crate) fn reviewed_head_rebased(&mut self, last: &str, head: &str) {
        if self.reviewed_from() != Some((last, head)) {
            return;
        }
        if let Some(note) = self.reviewing.rebased_note() {
            self.message(note);
        }
    }

    /// Whether you're idle, when that changed since the last call: for the
    /// review plugin.
    pub(crate) fn take_idle(&mut self) -> Option<bool> {
        self.reviewing.take_idle()
    }

    /// What the plugin pushed, about the review shown or not at all.
    pub(super) fn review_pushed(&mut self, push: crate::plugin::review::Push) {
        let effects = self.reviewing.pushed(push);
        self.review_effects(effects);
    }

    /// What the plugin said under run `id`.
    pub(super) fn review_event(&mut self, id: u64, said: Said) -> Option<Action> {
        let shown = Shown {
            revisions: self.selected.as_ref().map(|(r, _)| r),
            description: &self.description,
        };
        let effects =
            (self.reviewing).said((id, said), self.comments.as_mut(), &shown, Instant::now());
        self.review_effects(effects)
    }

    /// After an update moved the head, how many commits it moved by, when
    /// git could tell.
    pub(crate) fn review_updated(&mut self, count: Option<usize>) {
        let effects = self.reviewing.updated(count);
        self.review_effects(effects);
    }

    /// The review's part of the program status: a plugin run is work going
    /// on.
    pub(super) fn review_status(&self) -> Option<term::Status> {
        let work = self.reviewing.work()?;
        Some(term::Status {
            state: term::State::Working,
            message: Some(format!("{} · {}", work.doing, work.label)),
            progress: work.progress,
        })
    }

    /// What's left to say once the viewer is gone: drafts in a review
    /// session that weren't posted.
    pub(crate) fn unposted_note(&self) -> Option<String> {
        self.reviewing.unposted_note(self.comments.as_ref())
    }

    /// The review session the session pointer names: its comments file,
    /// the review's id and its head.
    pub(crate) fn session_pointer(&self) -> Option<(std::path::PathBuf, String, String)> {
        self.reviewing.session_pointer()
    }

    /// Input that review mode's overlays take, or `o` while a posted review's
    /// note shows; `Continue` when it's for the rest of the viewer.
    pub(super) fn review_input(&mut self, input: &Input) -> ControlFlow<Option<Action>> {
        if self.posting.is_some() {
            return ControlFlow::Break(self.post_input(input));
        }
        if self.reviews.is_some() {
            return ControlFlow::Break(self.review_picker_input(input));
        }
        match (self.reviewing.link(), input) {
            (Some(url), Input::Key(Key::Char(OPEN_POSTED), Mods::NONE)) => {
                ControlFlow::Break(Some(Action::Browse(url.to_owned())))
            }
            (Some(_), Input::Key(..)) => {
                self.reviewing.forget_link();
                ControlFlow::Continue(())
            }
            _ => ControlFlow::Continue(()),
        }
    }

    /// Closes the dialog, keeping its summary in the comments file for
    /// the next time.
    pub(super) fn close_post(&mut self) {
        if let Some(dialog) = self.posting.take() {
            let effects = session::keep_summary(self.comments.as_mut(), dialog.summary.text());
            self.review_effects(effects);
        }
    }

    fn post_input(&mut self, input: &Input) -> Option<Action> {
        let dialog = self.posting.as_mut()?;
        let (key, mods) = match input {
            Input::Paste(text) => {
                dialog.writing = true;
                dialog.summary.paste(text);
                return None;
            }
            Input::Key(key, mods) => (*key, *mods),
            _ => return None,
        };
        if key == Key::Char('c') && mods.contains(Mods::CTRL) {
            self.close_post();
            return Some(Action::Quit);
        }
        if dialog.writing {
            if key == Key::Esc {
                dialog.writing = false;
            } else {
                dialog.summary.key(key, mods);
            }
            return None;
        }
        match key {
            Key::Esc => self.close_post(),
            Key::Tab | Key::Char('s') => dialog.writing = true,
            Key::Up | Key::Char('k') => dialog.go_to(dialog.row.saturating_sub(1)),
            Key::Down | Key::Char('j') => dialog.go_to(dialog.row + 1),
            Key::Left | Key::Char('-') => dialog.vote(-1),
            Key::Right | Key::Char('+' | '=') => dialog.vote(1),
            Key::Char('l') => {
                dialog.local = !dialog.local;
                let local = dialog.local;
                if self.sending(true).local == 0 {
                    self.message("no local drafts on this diff");
                } else {
                    self.message(if local {
                        "the local drafts go with the review"
                    } else {
                        "the local drafts stay local"
                    });
                }
            }
            Key::Char(c) if dialog.events.iter().any(|e| e.key == c) => {
                dialog.event = dialog.events.iter().position(|e| e.key == c).unwrap_or(0);
                dialog.row = dialog.event;
            }
            Key::Char(GO_TO_UNPOSTED) => {
                let local = dialog.local;
                let left = self.reviewing.wont_go(self.comments.as_ref(), local);
                if left.is_empty() {
                    self.message("every draft can be posted");
                } else {
                    self.close_post();
                    let effect = Reviewing::go_to_unposted(self.comments.as_ref(), &left);
                    self.review_effects(vec![effect]);
                }
            }
            Key::Enter => return self.send_review(),
            _ => {}
        }
        None
    }

    /// Posts the review as the dialog has it; when the local drafts can't
    /// be moved, nothing is posted and the dialog stays.
    fn send_review(&mut self) -> Option<Action> {
        let dialog = self.posting.as_ref()?;
        let post = Post {
            event: dialog.events[dialog.event].clone(),
            labels: &dialog.labels,
            summary: dialog.summary.text(),
            local: dialog.local,
        };
        let effects = match self.reviewing.send(self.comments.as_mut(), post) {
            Ok(effects) => {
                self.posting = None;
                effects
            }
            Err(effects) => effects,
        };
        self.review_effects(effects)
    }

    fn review_picker_input(&mut self, input: &Input) -> Option<Action> {
        let picker = self.reviews.as_mut()?;
        let last = picker.offer.reviews.len() - 1;
        match *input {
            Input::Key(Key::Char('c'), mods) if mods.contains(Mods::CTRL) => {
                return Some(Action::Quit);
            }
            Input::Key(Key::Esc | Key::Char('q'), _) => self.reviews = None,
            Input::Key(Key::Up | Key::Char('k'), _) => {
                picker.cursor = picker.cursor.saturating_sub(1);
            }
            Input::Key(Key::Down | Key::Char('j'), _) => {
                picker.cursor = (picker.cursor + 1).min(last);
            }
            Input::Scroll { delta, .. } => {
                picker.cursor = picker
                    .cursor
                    .saturating_add_signed(isize::from(delta))
                    .min(last);
            }
            Input::Key(Key::Enter, _) => {
                let listed = &picker.offer.reviews[picker.cursor];
                let (id, shown) = (listed.id.clone(), listed.shown().to_owned());
                self.reviews = None;
                if self.piped {
                    self.message("a piped diff can't be reviewed");
                    return None;
                }
                let label = self.reviewing.label_of(&shown);
                let effects = self.reviewing.open(id, label);
                return self.review_effects(effects);
            }
            _ => {}
        }
        None
    }
}

impl Dialog {
    /// `checked` says where the plan the counts come from stands.
    pub(super) fn draw(
        &mut self,
        grid: &mut Grid,
        (theme, keys): (&Theme, &Keys),
        mode: &Mode,
        (sending, checked): (Sending, Checked),
    ) {
        let (gw, gh) = (grid.width(), grid.height());
        if gw < 20 || gh < 8 {
            return;
        }
        let width = if gw < help::FULL_SCREEN_BELOW {
            gw
        } else {
            64.min(gw - 4)
        };
        let dim = theme.style(Role::Dim);
        let warning = sending
            .warning()
            .map(|w| model::say(&w, |hint| super::hint_words(keys, hint)));
        let mut lines = self.heading(theme, mode, (sending, checked, warning));
        lines.push((String::new(), Style::PLAIN));
        let first_event = lines.len();
        for (i, event) in self.events.iter().enumerate() {
            let dot = if i == self.event { "(•)" } else { "( )" };
            lines.push((
                format!("{dot} {}  {}", event.key, event.label),
                Style::PLAIN,
            ));
        }
        for (label, at) in &self.labels {
            let vote = &label.values[*at];
            let value = match vote.value {
                v if v > 0 => format!("+{v}"),
                v => v.to_string(),
            };
            lines.push((
                format!("{}  ‹{value}›  {}", label.name, vote.label),
                Style::PLAIN,
            ));
        }
        lines.push((String::new(), Style::PLAIN));
        let hint = if self.writing {
            "summary · esc back"
        } else {
            "summary · s writes"
        };
        lines.push((hint.to_owned(), dim));
        let summary_at = lines.len();
        let rows = self.summary.rows(width - 6);
        let shown = rows.len().clamp(1, 6);
        let skip = rows
            .iter()
            .position(|(_, c)| c.is_some())
            .map_or(0, |at| (at + 1).saturating_sub(shown));
        let footer = if self.writing {
            "enter new line · esc done"
        } else if self.labels.is_empty() {
            "enter sends · j/k or the letter picks · esc closes"
        } else {
            "enter sends · j/k or the letter picks · ←/→ votes · esc closes"
        };
        let height = (lines.len() + shown + 4).min(gh);
        let (x0, y0) = ((gw - width) / 2, (gh - height) / 2);
        let title = format!(" post · {} ", mode.label);
        help::frame(grid, (x0, y0, width, height), &title, theme);
        let (x, right) = (x0 + 2, x0 + width - 2);
        for (i, (text, style)) in lines.iter().enumerate() {
            let y = y0 + 1 + i;
            if y + 2 >= y0 + height {
                break;
            }
            grid.put_str(x, y, right, text, *style);
            if i.checked_sub(first_event) == Some(self.row) {
                let mut bold = theme.selected(Style::PLAIN, !self.writing);
                bold.attributes |= Attributes::BOLD;
                grid.map_styles(x, y, right, |_| bold);
            }
        }
        for (i, (text, cursor)) in rows.iter().skip(skip).take(shown).enumerate() {
            let y = y0 + 1 + summary_at + i;
            if y + 2 >= y0 + height {
                break;
            }
            let end = grid.put_str(x + 2, y, right, text, Style::PLAIN);
            if let Some(at) = cursor
                && self.writing
            {
                let column = x + 2 + text[..*at].width();
                grid.map_styles(column, y, column + 1, |s| theme.selected(s, true));
                if column >= end {
                    grid.put_str(column, y, right, " ", theme.selected(Style::PLAIN, true));
                }
            }
        }
        grid.put_str(x, y0 + height - 2, right, footer, dim);
    }

    /// The lines over the events: what goes, the one warning about what
    /// won't, the review's state, and the local drafts' toggle.
    fn heading(
        &self,
        theme: &Theme,
        mode: &Mode,
        (sending, checked, warning): (Sending, Checked, Option<String>),
    ) -> Vec<(String, Style)> {
        let (dim, warn) = (theme.style(Role::Dim), theme.style(Role::Warn));
        let mut lines = vec![(sending.what(&mode.label), Style::PLAIN)];
        match (warning, checked) {
            (Some(warning), _) => lines.push((format!("! {warning}"), warn)),
            (None, Checked::Failed(e)) => lines.push((
                format!(
                    "couldn't check what can be posted: {}",
                    escape(e.as_bytes())
                ),
                dim,
            )),
            (None, Checked::Checking) if sending.comments > 0 => {
                lines.push(("checking what can be posted…".to_owned(), dim));
            }
            (None, _) => {}
        }
        if mode.moved.is_some() {
            lines.push((
                format!(
                    "! {} has new commits; this review goes on the head shown",
                    mode.label
                ),
                warn,
            ));
        }
        if let Some(state) = mode.ended() {
            lines.push((format!("! {} is {}", mode.label, state.word()), warn));
        }
        if sending.local > 0 {
            let mark = if self.local { "[x]" } else { "[ ]" };
            lines.push((
                format!(
                    "l  {mark} with the {} on this diff",
                    plural(sending.local, "local draft")
                ),
                Style::PLAIN,
            ));
        }
        lines
    }

    /// Whether the local drafts in view go with the review.
    pub(super) fn with_local(&self) -> bool {
        self.local
    }

    /// Moves j/k's line to `row`, kept to the events and the labels; on an
    /// event, picks it.
    fn go_to(&mut self, row: usize) {
        self.row = row.min(self.events.len() + self.labels.len() - 1);
        if self.row < self.events.len() {
            self.event = self.row;
        }
    }

    /// On a label, its vote `by` values on, kept to its values.
    fn vote(&mut self, by: isize) {
        let label = self.row.checked_sub(self.events.len());
        let Some((label, at)) = label.and_then(|i| self.labels.get_mut(i)) else {
            return;
        };
        *at = at.saturating_add_signed(by).min(label.values.len() - 1);
    }
}

impl Picker {
    pub(super) fn draw(&mut self, grid: &mut Grid, theme: &Theme) {
        let (gw, gh) = (grid.width(), grid.height());
        if gw < 20 || gh < 6 {
            return;
        }
        let mut lines: Vec<Option<usize>> = Vec::new();
        let mut titles: Vec<&str> = Vec::new();
        let mut last = None;
        for (i, &group) in self.offer.group.iter().enumerate() {
            if last != Some(group) {
                if last.is_some() {
                    lines.push(None);
                    titles.push("");
                }
                lines.push(None);
                titles.push(&self.offer.groups[group]);
                last = Some(group);
            }
            lines.push(Some(i));
            titles.push("");
        }
        let full = gw < help::FULL_SCREEN_BELOW;
        let width = if full { gw } else { 80.min(gw - 4) };
        let height = if full {
            gh
        } else {
            (lines.len() + 4).min(gh - 2)
        };
        let (x0, y0) = ((gw - width) / 2, (gh - height) / 2);
        help::frame(grid, (x0, y0, width, height), &self.title, theme);
        let rows = height - 3;
        let at = lines
            .iter()
            .position(|l| *l == Some(self.cursor))
            .unwrap_or(0);
        if at < self.scroll {
            self.scroll = at.saturating_sub(1);
        } else if at >= self.scroll + rows {
            self.scroll = at + 1 - rows;
        }
        let (x, right) = (x0 + 2, x0 + width - 3);
        let dim = theme.style(Role::Dim);
        for (row, (line, title)) in lines
            .iter()
            .zip(&titles)
            .enumerate()
            .skip(self.scroll)
            .take(rows)
        {
            let y = y0 + 1 + row - self.scroll;
            let Some(i) = *line else {
                let mut bold = theme.style(Role::Plain);
                bold.attributes |= Attributes::BOLD;
                grid.put_str(x, y, right, title, bold);
                continue;
            };
            let listed = &self.offer.reviews[i];
            let selected = i == self.cursor;
            let mark = |s: Style| if selected { theme.selected(s, true) } else { s };
            if selected {
                grid.fill(x - 1, y, right + 1, mark(Style::PLAIN));
            }
            let after = grid.put_str(
                x,
                y,
                right,
                &format!("{:<w$}", self.offer.tags[i], w = self.tag_width),
                mark(theme.style(Role::Hint)),
            );
            let drafts = match listed.drafts {
                0 => String::new(),
                n => format!("  {}", plural(n, "draft")),
            };
            let author = format!("  {}", escape(listed.author.as_bytes()));
            let title_end = right.saturating_sub(author.width() + drafts.width());
            grid.put_str(
                after + 1,
                y,
                title_end,
                &escape(listed.title.as_bytes()),
                mark(Style::PLAIN),
            );
            let end = grid.put_str(title_end, y, right, &author, mark(dim));
            grid.put_str(end, y, right, &drafts, mark(theme.style(Role::Warn)));
        }
        help::scrollbar(
            grid,
            (x0 + width - 2, y0 + 1, rows),
            (lines.len(), self.scroll),
            dim,
        );
        grid.put_str(
            x,
            y0 + height - 2,
            right,
            "enter opens · j/k move · esc close",
            dim,
        );
    }
}

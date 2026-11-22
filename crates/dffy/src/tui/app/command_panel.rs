//! The command panel the leader key opens: its entries with their states,
//! where each is listed, and its keys.

use dffy_core::diff::Whitespace;

use super::{Action, App, Pane, Watch};
use crate::config::{self, View};
use crate::keys::{Action as Act, Tab, Table};
use crate::term::Input;
use crate::tui::commands::{self, Commands, Outcome, Place};
use crate::tui::menu::Offer;

/// Here's actions on a thread, in this order, as far as they act on it.
const ON_THREAD: [Act; 6] = [
    Act::AskAgent,
    Act::FixWithAgent,
    Act::ApplySuggestion,
    Act::CopyPrompt,
    Act::Resolve,
    Act::Reply,
];

/// Here's actions on checkpoints, once one is taken.
const CHECKPOINTS: [Act; 2] = [Act::SinceCheckpoint, Act::TakeCheckpoint];

/// Here's actions with nothing more particular to act on.
const ANYWHERE: [Act; 5] = [
    Act::OpenPicker,
    Act::CycleLayout,
    Act::ToggleWrap,
    Act::ToggleWatch,
    Act::OpenTerminal,
];

impl App {
    /// Opens the command panel's quick popup, its entries as things stand.
    pub(super) fn open_commands(&mut self) {
        let (title, entries) = self.command_entries();
        self.commands = Some(Commands::new(title, entries));
    }

    /// Input while the command panel is open: a row's key or `enter` runs
    /// it, toggles keeping the panel open with their new state flashing;
    /// see [`Commands`] for the rest.
    pub(super) fn commands_input(&mut self, input: &Input) -> Option<Action> {
        let commands = self.commands.as_mut()?;
        let outcome = match *input {
            Input::Key(key, mods) => commands.key(key, mods),
            Input::Scroll { delta, .. } => commands.scroll(isize::from(delta)),
            Input::Click { x, y } => commands.tap((usize::from(x), usize::from(y))),
            _ => Outcome::Stay,
        };
        let ran = matches!(outcome, Outcome::Run { .. } | Outcome::Theme { .. });
        let done = self.command_outcome(outcome);
        if ran && self.commands.is_some() {
            let (title, entries) = self.command_entries();
            if let Some(commands) = &mut self.commands {
                commands.update(title, entries);
            }
        }
        done
    }

    /// Carries out what a key or tap in the panel asked for.
    fn command_outcome(&mut self, outcome: Outcome) -> Option<Action> {
        match outcome {
            Outcome::OpenThemes => {
                if !self.color {
                    self.message("colors are off; themes don't show");
                    return None;
                }
                self.load_themes();
                let names = self
                    .themes
                    .iter()
                    .map(|(n, t)| (n.clone(), t.group()))
                    .collect();
                if let Some(commands) = &mut self.commands {
                    commands.open_themes(names, &self.theme_name);
                }
                None
            }
            Outcome::Theme { name, keep } => {
                self.theme_name = name;
                self.reload_theme();
                if keep {
                    self.remember("theme", self.theme_name.clone());
                    if let Some(commands) = &mut self.commands {
                        commands.flash(Act::CycleTheme);
                    }
                }
                None
            }
            Outcome::Stay => None,
            Outcome::Close => {
                self.commands = None;
                None
            }
            Outcome::Unknown => {
                self.message("no command on that key; esc closes");
                None
            }
            Outcome::Run { action, back } => {
                let stays = (self.commands.as_ref()).is_some_and(|c| c.stays(action));
                if !stays {
                    self.commands = None;
                }
                // Backward is forward round the cycle, short of one, but
                // for themes, whose cycle is long.
                let times = if back {
                    Self::cycle_len(action).saturating_sub(1)
                } else {
                    1
                };
                let mut done = None;
                if action == Act::CycleTheme {
                    self.step_theme(back);
                } else {
                    for _ in 0..times.max(1) {
                        done = self.act(action);
                    }
                }
                if let Some(note) = self.state_note(action) {
                    self.message(note);
                }
                if let Some(commands) = &mut self.commands {
                    commands.flash(action);
                }
                done
            }
        }
    }

    /// How many values a multi-value setting goes through.
    fn cycle_len(action: Act) -> usize {
        match action {
            Act::CycleLayout => 3,
            Act::CycleIndicators => 4,
            Act::CycleModifiedLines | Act::ToggleStatusBar => 2,
            _ => 1,
        }
    }

    /// The quick popup's Here title and the command panel's rows as things
    /// stand: Here's, then the leader's table, each with its state, its key
    /// in the panel and the key that does the same outside it.
    pub(super) fn command_entries(&mut self) -> (String, Vec<commands::Entry>) {
        let (title, here) = self.here();
        let mut entries: Vec<commands::Entry> = (here.into_iter())
            .map(|offer| {
                let key = (self.keys.leader_entries().iter())
                    .find(|(_, a)| *a == offer.action)
                    .map(|(chord, _)| chord.name())
                    .unwrap_or_default();
                let keys = (key, self.direct_key(offer.action, offer.table));
                self.command_entry(offer.action, Some(offer.label), keys, Place::Here)
            })
            .collect();
        for &(chord, action) in self.keys.leader_entries() {
            let place = match action.group().tab() {
                _ if !self.applies(action) => Place::Hidden,
                Tab::General => Place::Here,
                Tab::Move => Place::Tab(Tab::Files),
                tab => Place::Tab(tab),
            };
            let keys = (chord.name(), self.direct_key(action, Table::Main));
            entries.push(self.command_entry(action, None, keys, place));
        }
        (title, entries)
    }

    /// A row for `action` listed at `place`, its words `label` or else the
    /// panel's own, with its key in the panel, empty for none, and its key
    /// outside it.
    fn command_entry(
        &self,
        action: Act,
        label: Option<String>,
        (key, direct): (String, Option<String>),
        place: Place,
    ) -> commands::Entry {
        let (on, title, value) = self.panel_state(action);
        let setting = on.is_some() || value.is_some();
        commands::Entry {
            action,
            direct: direct.filter(|d| *d != key),
            key,
            on,
            label: label.unwrap_or(title),
            value,
            // These go somewhere, rather than set a look.
            stays: setting
                && !matches!(
                    action,
                    Act::ToggleTree
                        | Act::CommentedFiles
                        | Act::SinceLastReview
                        | Act::SinceCheckpoint
                ),
            place,
        }
    }

    /// The key that does `action` outside the panel, in `table`: a
    /// thread's or a conflict's, else the diff's or the tree's.
    fn direct_key(&self, action: Act, table: Table) -> Option<String> {
        match table {
            Table::Thread => self.thread_key_name(action),
            Table::Conflict => {
                (self.keys.conflict_chords(action).next()).map(crate::keys::Chord::name)
            }
            _ => self
                .key_name(action)
                .or_else(|| (self.keys.chords(true, action).next()).map(crate::keys::Chord::name)),
        }
    }

    /// What the panel's Here tab offers, from what the keys act on now: on
    /// lines selected in the diff, what they do to them; on a thread, a
    /// few of its keys; in a conflict on screen, its choices; on a review,
    /// its actions; else a few settings and actions. Sending your open
    /// threads to your agent joins a thread's, a review's and the rest
    /// while you have some. With them, what they act on, the title of the
    /// quick popup's Here group: `On this thread`, or `Here` with nothing
    /// in particular.
    fn here(&mut self) -> (String, Vec<Offer>) {
        if self.pane == Pane::Diff
            && let Some((file, ..)) = self.doc.selected_lines()
        {
            let mut offers = self.line_offers(file, true);
            offers.extend(self.edit_offers(file, true));
            return ("On these lines".to_owned(), offers);
        }
        let send = (self.applies(Act::SendOpenThreads)).then(|| {
            Offer::new(
                Act::SendOpenThreads.title(),
                Act::SendOpenThreads,
                Table::Main,
            )
        });
        if let Some(thread) = self.target_thread() {
            let offers = self.thread_offers(&thread, false);
            let offers = (ON_THREAD.iter())
                .filter_map(|&a| offers.iter().find(|o| o.action == a).cloned())
                .chain(send)
                .collect();
            return ("On this thread".to_owned(), offers);
        }
        // The conflict a choice would act on: the line last tapped's first.
        let tapped = self.tapped_shown().map(|(pos, _)| pos);
        let conflict = self.conflict_offers(tapped);
        if !conflict.is_empty() {
            return ("In this conflict".to_owned(), conflict);
        }
        let review = self.review_offers();
        if !review.is_empty() {
            let title = format!("On this {}", self.reviewing.noun(false));
            return (title, review.into_iter().chain(send).collect());
        }
        // The agent loop first: your open threads to hand over, then once
        // there's a checkpoint, its toggle and the next checkpoint.
        let checkpoints = self.checkpoint.is_some().then_some(CHECKPOINTS);
        let offers = (std::iter::once(Act::SendOpenThreads))
            .chain(checkpoints.into_iter().flatten())
            .chain(ANYWHERE)
            .filter(|&a| self.applies(a))
            .map(|a| Offer::new(a.title(), a, Table::Main))
            .collect();
        ("Here".to_owned(), offers)
    }
    /// A panel row's checkbox, if it's a toggle, its text, and a
    /// multi-value setting's value (`auto (split here)`).
    fn panel_state(&self, action: Act) -> (Option<bool>, String, Option<String>) {
        let title = action.title();
        let value = |v: String| (None, title.to_owned(), Some(v));
        let toggle = |on: bool| (Some(on), title.to_owned(), None);
        match action {
            Act::CycleLayout => {
                let here = if self.splits() { "split" } else { "unified" };
                let view = config::choice_name(&self.view);
                if view == here {
                    value(view)
                } else {
                    value(format!("{view} ({here} here)"))
                }
            }
            Act::CycleIndicators => value(config::choice_name(&self.indicators)),
            Act::CycleModifiedLines => value(config::choice_name(&self.modified_lines)),
            Act::CycleTheme => value(self.theme_name.clone()),
            Act::ToggleStatusBar => value(config::choice_name(&self.status_bar)),
            Act::ToggleWrap => toggle(self.wrap),
            Act::ToggleSyntax => toggle(self.doc.document.structural),
            Act::ToggleRendered => toggle(self.doc.document.rendered()),
            Act::ToggleHighlight => toggle(self.highlight),
            Act::ToggleWhitespace => toggle(self.whitespace != Whitespace::Exact),
            Act::ToggleLineNumbers => toggle(self.line_numbers),
            Act::ToggleHunkHeaders => toggle(self.hunk_headers),
            Act::ToggleCopyDecorations => toggle(self.copy_decorations),
            Act::ToggleMouse => toggle(self.mouse),
            Act::ToggleWatch => toggle(self.watch != Watch::Off),
            Act::ToggleTree => toggle(self.tree_shown),
            Act::CommentedFiles => toggle(self.tree.commented()),
            Act::SinceLastReview => toggle(self.reviewing.since_review()),
            Act::SinceCheckpoint => toggle(self.since_checkpoint.is_some()),
            Act::WholeReview => (None, self.reviewing.whole_title(), None),
            Act::OpenReview => (None, format!("Open {}…", self.reviewing.noun(false)), None),
            Act::ToggleFull => toggle(
                self.doc
                    .document
                    .files()
                    .get(self.doc.document.pos.file)
                    .is_some_and(|e| self.doc.document.is_full(e)),
            ),
            _ => (None, title.to_owned(), None),
        }
    }

    /// The note after a command from the menu: the state it set, said
    /// plainly (`syntax diff on`, `layout: auto (split at this width)`).
    pub(super) fn state_note(&self, action: Act) -> Option<String> {
        let on = |on: bool| if on { "on" } else { "off" };
        let label = action.label();
        Some(match action {
            Act::CycleLayout => {
                let now = if self.splits() { "split" } else { "unified" };
                match self.view {
                    View::Auto => format!("layout: auto ({now} at this width)"),
                    View::Split if now == "unified" => {
                        "layout: split (unified at this width)".to_owned()
                    }
                    view => format!("layout: {}", config::choice_name(&view)),
                }
            }
            Act::CycleIndicators
            | Act::CycleModifiedLines
            | Act::CycleTheme
            | Act::ToggleStatusBar => {
                let (_, label, value) = self.panel_state(action);
                format!("{}: {}", label.to_lowercase(), value.unwrap_or_default())
            }
            Act::ToggleWrap => format!("{label} {}", on(self.wrap)),
            Act::ToggleSyntax => format!("{label} {}", on(self.doc.document.structural)),
            Act::ToggleHighlight => format!("{label} {}", on(self.highlight)),
            Act::ToggleWhitespace => {
                format!("{label} {}", on(self.whitespace != Whitespace::Exact))
            }
            Act::ToggleLineNumbers => format!("{label} {}", on(self.line_numbers)),
            Act::ToggleHunkHeaders => format!("{label} {}", on(self.hunk_headers)),
            Act::ToggleCopyDecorations => format!("{label} {}", on(self.copy_decorations)),
            Act::ToggleWatch => format!("{label} {}", on(self.watch != Watch::Off)),
            Act::ToggleTree => format!(
                "{label} {}",
                if self.tree_shown { "shown" } else { "hidden" }
            ),
            _ => return None,
        })
    }
}

//! The command panel the leader key opens: its entries with their states,
//! and its keys.

use dffy_core::diff::Whitespace;

use super::{Action, App, Watch};
use crate::config::{self, View};
use crate::keys::Action as Act;
use crate::term::Input;
use crate::tui::commands::{self, Outcome};

impl App {
    /// Input while the command panel is open: a row's key or `enter` runs
    /// it, toggles keeping the panel open with their new state flashing;
    /// see [`Commands`] for the rest.
    pub(super) fn commands_input(&mut self, input: &Input) -> Option<Action> {
        let entries = self.command_entries();
        let commands = self.commands.as_mut()?;
        let outcome = match *input {
            Input::Key(key, mods) => commands.key(&entries, key, mods),
            Input::Scroll { delta, .. } => commands.scroll(&entries, isize::from(delta)),
            Input::Click { x, y } => commands.tap(&entries, (usize::from(x), usize::from(y))),
            _ => Outcome::Stay,
        };
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
                let stays = entries.iter().any(|e| e.action == action && e.stays);
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

    /// The command panel's rows as things stand: the leader's table, each
    /// with its state and the key that does the same directly.
    pub(super) fn command_entries(&self) -> Vec<commands::Entry> {
        self.keys
            .leader_entries()
            .iter()
            .map(|&(chord, action)| {
                let key = chord.name();
                let direct = self
                    .key_name(action)
                    .or_else(|| {
                        self.keys
                            .chords(true, action)
                            .next()
                            .map(crate::keys::Chord::name)
                    })
                    .filter(|d| *d != key);
                let (on, label, value) = self.panel_state(action);
                let setting = on.is_some() || value.is_some();
                commands::Entry {
                    action,
                    key,
                    on,
                    label,
                    value,
                    direct,
                    // These go somewhere, rather than set a look.
                    stays: setting
                        && !matches!(
                            action,
                            Act::ToggleTree | Act::CommentedFiles | Act::SinceLastReview
                        ),
                }
            })
            .collect()
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

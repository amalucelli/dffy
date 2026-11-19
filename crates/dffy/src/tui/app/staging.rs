//! Staging, unstaging and discarding lines of the uncommitted changes
//! shown (`A`, `U`, `D` and their menu entries). The loop runs the git
//! operations, one at a time, and says how each went; a discard asks
//! first, once the loop has worked out which lines it takes.

use std::sync::Arc;

use dffy_core::git::{Applied, Edit, FileChange, FileDiff, Picked, Status};
use dffy_core::view::Staging;

use super::{Action, App, Event, STILL_EDITING, Uncommitted, escape, menu, session};
use crate::keys::Action as Act;

impl App {
    /// The uncommitted changes shown, if that's what the diff is now: not
    /// a review's range a review plugin switched to.
    fn uncommitted_shown(&self) -> Option<Uncommitted> {
        self.uncommitted
            .filter(|_| !self.piped && self.reviewing.shows_plain())
    }

    /// Where `change`'s lines are, as `which` shows them.
    fn staging_of(which: Uncommitted, change: &FileChange) -> Staging {
        match which {
            Uncommitted::Plain => change.staging.unwrap_or(Staging::Unstaged),
            Uncommitted::Staged => Staging::Staged,
            Uncommitted::Unstaged => Staging::Unstaged,
        }
    }

    /// `A`, `U` and `D`: the selected lines, else the hunk of the line last
    /// tapped or of the first on screen, go to the loop to stage, unstage
    /// or discard, as far as the file's staging leaves anything to do.
    /// Discarding asks first, once the loop has worked out what it takes
    /// ([`Self::discard_planned`]).
    pub(super) fn edit_lines(&mut self, edit: Edit) -> Option<Action> {
        let Some(which) = self.uncommitted_shown() else {
            self.message(
                "staging works on uncommitted changes: plain dffy, --staged or --unstaged",
            );
            return None;
        };
        // Before the selection goes, so the next press can take it.
        if self.editing {
            self.message(STILL_EDITING);
            return None;
        }
        let tapped = self.take_tapped();
        let target = self.doc.edit_target(tapped);
        self.doc.selection = None;
        let (file, picked) = match target {
            Ok(target) => target,
            Err(why) => {
                self.message(why);
                return None;
            }
        };
        let diff = Arc::clone(self.doc.document.diff_of(file)?);
        let refused = match (edit, Self::staging_of(which, &diff.change)) {
            (Edit::Unstage, Staging::Unstaged) => Some("nothing here is staged"),
            (Edit::Stage, Staging::Staged) => Some("this is staged already"),
            _ => None,
        };
        if let Some(why) = refused {
            self.message(why);
            return None;
        }
        if let Err(e) = edit.check(&diff) {
            self.message(e.to_string());
            return None;
        }
        if picked.changed(&diff) == 0 {
            self.message(format!("no changed lines here to {}", edit.verb()));
            return None;
        }
        Some(match edit {
            Edit::Discard => Action::PlanDiscard { diff, picked },
            Edit::Stage | Edit::Unstage => Action::EditLines { diff, picked, edit },
        })
    }

    /// A git operation the loop ran said how it went: the next can run.
    pub(super) fn edit_done(&mut self, event: Event) {
        self.editing = false;
        match event {
            Event::Edited(Ok(note)) => self.message(note),
            Event::Edited(Err(why)) => self.failed(why),
            Event::Planned {
                diff,
                picked,
                result,
            } => self.discard_planned(diff, picked, result),
            Event::Resolved { path, result } => self.resolved(path, result),
            _ => {}
        }
    }

    /// What discarding `picked` lines of `diff` takes, as the loop worked
    /// it out: the question before it, naming the lines that go.
    fn discard_planned(
        &mut self,
        diff: Arc<FileDiff>,
        picked: Picked,
        result: Result<Applied, String>,
    ) {
        let applied = match result {
            Ok(applied) => applied,
            Err(why) => {
                self.failed(why);
                return;
            }
        };
        // A `y` meant for something opened since would answer the question.
        if self.discard_asked.take() != Some(self.inputs) {
            self.message("discard dropped: other input came before its question");
            return;
        }
        let path = escape(&diff.change.path);
        let prompt = if applied.deleted {
            format!("delete the untracked file {path}? y/n")
        } else {
            let at = match applied.at {
                Some((first, last)) if first == last => format!(" at line {first}"),
                Some((first, last)) => format!(" at lines {first}-{last}"),
                None => String::new(),
            };
            format!(
                "discard {}{at} of {path}? y/n",
                session::plural(applied.lines, "line")
            )
        };
        let action = Action::EditLines {
            diff,
            picked,
            edit: Edit::Discard,
        };
        self.message = None;
        self.confirm = Some((prompt, action, "kept"));
        self.dirty = true;
    }

    /// The right-click menu's entries staging, unstaging and discarding
    /// on `file`: those its staging leaves anything to do for.
    pub(super) fn edit_entries(&self, file: usize, selected: bool) -> Vec<menu::Entry> {
        let (Some(which), Some(entry)) = (
            self.uncommitted_shown(),
            self.doc.document.files().get(file),
        ) else {
            return Vec::new();
        };
        // A conflicted file's lines aren't staged: its conflicts are resolved.
        if entry.is_commit() || entry.change().status == Status::Unmerged {
            return Vec::new();
        }
        let staging = Self::staging_of(which, entry.change());
        let what = if selected { "selected lines" } else { "hunk" };
        let mut entries = Vec::new();
        if staging != Staging::Staged {
            entries.push(self.menu_entry(false, &format!("Stage {what}"), Act::Stage));
        }
        if staging != Staging::Unstaged {
            entries.push(self.menu_entry(false, &format!("Unstage {what}"), Act::Unstage));
        }
        if staging != Staging::Staged {
            entries.push(self.menu_entry(false, &format!("Discard {what}…"), Act::Discard));
        }
        entries
    }
}

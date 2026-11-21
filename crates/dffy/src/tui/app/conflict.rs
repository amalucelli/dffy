//! Resolving a conflicted file in the viewer. What a conflict is, what a
//! choice keeps, the preview of the choices and the file written are the
//! core's ([`dffy_core::conflict`], [`dffy_core::document::Conflicted`]);
//! here keys and the right-click menu pick the conflict and the choice,
//! and the write and `git add` go to the loop as actions.

use std::ops::ControlFlow;

use dffy_core::conflict::{Choice, Region};
use dffy_core::document::Pos;
use dffy_core::git::Status;

use super::{Action, App, Confirmed, escape};
use crate::keys::{Action as Act, Table};
use crate::review::session::plural;
use crate::tui::menu::Offer;

/// How a choice reads once made.
fn made(choice: Choice) -> &'static str {
    match choice {
        Choice::Ours => "current kept",
        Choice::Theirs => "incoming taken",
        Choice::Both => "both kept",
        Choice::Base => "base taken",
    }
}

impl App {
    /// The regions of `file`, or why it has none to resolve.
    fn conflict_regions(&self, file: usize) -> Result<Vec<Region>, String> {
        let entry = self
            .doc
            .document
            .files()
            .get(file)
            .filter(|e| !e.is_commit())
            .ok_or("no conflicts here")?;
        let loaded = entry.file().ok_or("this file isn't diffed yet")?;
        let conflicted = loaded.conflicted().ok_or_else(|| {
            if entry.change().status == Status::Unmerged {
                "conflicts are resolved in the working tree: plain dffy shows it"
            } else {
                "no conflicts in this file"
            }
        })?;
        if let Some(why) = conflicted.refused() {
            return Err(why.to_owned());
        }
        if conflicted.regions().is_empty() {
            return Err("no conflict markers left in this file".to_owned());
        }
        Ok(conflicted.regions().to_vec())
    }

    /// Whether the file at the cursor has conflicts to resolve, or one it
    /// can't resolve here, which says why: either puts the conflict's keys
    /// before the diff's.
    pub(super) fn has_conflicts(&self) -> bool {
        let entry = self.doc.document.files().get(self.doc.document.pos.file);
        (entry.filter(|e| !e.is_commit()))
            .and_then(|e| e.file()?.conflicted())
            .is_some_and(|c| c.refused().is_some() || !c.regions().is_empty())
    }

    /// Arriving in a file with conflicts to resolve says, once, which of
    /// how many is on screen and the keys that choose: `conflict 1/5 · o
    /// ours · t theirs · b both · w writes`. Leaving it, coming back says
    /// it again.
    pub(super) fn arrive_conflicts(&mut self) {
        let file = self.doc.document.pos.file;
        if self.conflicts_said.is_some_and(|said| said != file) {
            self.conflicts_said = None;
        }
        if self.conflicts_said.is_some() || !self.has_conflicts() {
            return;
        }
        let Ok(count) = self.conflict_regions(file).map(|r| r.len()) else {
            return;
        };
        self.conflicts_said = Some(file);
        let at = self.doc.conflicts_at(None).first().map_or(0, |r| *r);
        let note = format!(
            "conflict {}/{count} · {} ours · {} theirs · {} both · {} writes",
            at + 1,
            self.conflict_key(Act::KeepCurrent),
            self.conflict_key(Act::TakeIncoming),
            self.conflict_key(Act::KeepBoth),
            self.conflict_key(Act::WriteResolution),
        );
        self.message(note);
    }

    /// The first key the conflict's table binds `action` to, else the
    /// diff's, else its name.
    pub(super) fn conflict_key(&self, action: Act) -> String {
        (self.keys.conflict_chords(action).next())
            .or_else(|| self.keys.chords(false, action).next())
            .map_or_else(|| action.name().to_owned(), crate::keys::Chord::name)
    }

    /// Does `action`, one of the conflict's. `Continue` for the next or
    /// previous conflict when there's none that way, so the key does what
    /// the diff binds it to: `n` goes on to the next change.
    pub(super) fn conflict_act(&mut self, action: Act) -> ControlFlow<Option<Action>> {
        match action {
            Act::NextConflict | Act::PrevConflict => {
                if !self.step_conflict(action == Act::NextConflict) {
                    return ControlFlow::Continue(());
                }
            }
            Act::KeepCurrent => self.choose_conflict(Choice::Ours),
            Act::TakeIncoming => self.choose_conflict(Choice::Theirs),
            Act::KeepBoth => self.choose_conflict(Choice::Both),
            Act::TakeBase => self.choose_conflict(Choice::Base),
            Act::WriteResolution => return ControlFlow::Break(self.write_resolution()),
            _ => return ControlFlow::Continue(()),
        }
        ControlFlow::Break(None)
    }

    /// The conflict the next or previous conflict key or a choice last
    /// went to, while it's on screen.
    fn current_conflict(&mut self) -> Option<usize> {
        let (file, region) = self.conflict?;
        (file == self.doc.document.pos.file && self.doc.conflicts_at(None).contains(&region))
            .then_some(region)
    }

    /// The next or previous conflict of the file shown, from the one last
    /// gone to while it's on screen, else from the top. False, saying
    /// nothing, when there's none that way.
    pub(super) fn step_conflict(&mut self, forward: bool) -> bool {
        let file = self.doc.document.pos.file;
        let Ok(count) = self.conflict_regions(file).map(|r| r.len()) else {
            return false;
        };
        let top = Pos {
            offset: 0,
            ..self.doc.document.pos
        };
        let next = match self.current_conflict() {
            Some(at) if forward => (at + 1 < count).then_some(at + 1),
            Some(at) => at.checked_sub(1),
            None if forward => {
                (0..count).find(|&r| self.doc.conflict_pos(r).is_some_and(|p| p >= top))
            }
            None => (0..count)
                .rev()
                .find(|&r| self.doc.conflict_pos(r).is_some_and(|p| p < top)),
        };
        let Some(next) = next else {
            return false;
        };
        self.doc.selection = None;
        self.doc.goto_conflict(next);
        self.conflict = Some((file, next));
        // A diff on its way puts the view on this conflict, not the one
        // chosen before.
        if let Some(anchor) = self.conflict_anchor.as_mut().filter(|(f, _)| *f == file) {
            anchor.1 = next;
        }
        let chosen = self.doc.document.files()[file]
            .choices()
            .and_then(|c| c.picks.get(next).copied().flatten());
        let note = match chosen {
            Some(choice) => format!("conflict {}/{count}: {}", next + 1, made(choice)),
            None => format!(
                "conflict {}/{count} · {} keeps current · {} takes incoming · {} both",
                next + 1,
                self.conflict_key(Act::KeepCurrent),
                self.conflict_key(Act::TakeIncoming),
                self.conflict_key(Act::KeepBoth),
            ),
        };
        self.message(note);
        true
    }

    /// Keep current, take incoming, keep both or take base: `choice` on
    /// the conflict of the line last tapped, else the one last gone to while it's on screen, else
    /// the first on screen; the same choice again takes it back. The
    /// file's diff then comes again as the preview of its choices, and the
    /// view goes back to the conflict once it has.
    pub(super) fn choose_conflict(&mut self, choice: Choice) {
        let tapped = self.take_tapped().map(|(pos, _)| pos);
        let file = tapped.map_or(self.doc.document.pos.file, |p| p.file);
        let regions = match self.conflict_regions(file) {
            Ok(regions) => regions,
            Err(why) => {
                self.message(why);
                return;
            }
        };
        let Some(region) = self.aimed_conflict(tapped) else {
            let note = if tapped.is_some() {
                "no conflict on this line".to_owned()
            } else {
                format!(
                    "no conflict on screen; {} goes to the next",
                    self.conflict_key(Act::NextConflict)
                )
            };
            self.message(note);
            return;
        };
        let before = self.doc.document.files()[file]
            .choices()
            .and_then(|c| c.picks.get(region).copied().flatten());
        let pick = (before != Some(choice)).then_some(choice);
        match self.doc.document.choose(file, region, pick) {
            Ok(choices) => {
                self.conflict = Some((file, region));
                self.conflict_anchor = Some((file, region));
                let count = regions.len();
                let left = count - choices.map_or(0, |c| c.chosen());
                let what = pick.map_or("choice taken back", made);
                let left = if left == 0 {
                    "all chosen".to_owned()
                } else {
                    format!("{left} left")
                };
                let write = self.conflict_key(Act::WriteResolution);
                self.message(format!(
                    "conflict {}/{count}: {what} · {left} · {write} writes",
                    region + 1
                ));
            }
            Err(why) => self.message(why),
        }
    }

    /// Writes the file shown as its choices resolve it, asking first when
    /// some conflicts have none, as they keep their markers.
    pub(super) fn write_resolution(&mut self) -> Option<Action> {
        let file = self.doc.document.pos.file;
        let count = match self.conflict_regions(file) {
            Ok(regions) => regions.len(),
            Err(why) => {
                self.message(why);
                return None;
            }
        };
        let entry = &self.doc.document.files()[file];
        let Some(choices) = entry.choices().cloned() else {
            let keys = [
                Act::KeepCurrent,
                Act::TakeIncoming,
                Act::KeepBoth,
                Act::TakeBase,
            ]
            .map(|a| self.conflict_key(a));
            self.message(format!(
                "nothing chosen to write; {}, {}, {} or {} chooses",
                keys[0], keys[1], keys[2], keys[3]
            ));
            return None;
        };
        let path = entry.change().path.clone();
        let left = count - choices.chosen();
        let prompt = format!("write with {left} of {count} conflicts unchosen, markers kept? y/n");
        let action = Action::WriteResolution { path, choices };
        if left == 0 {
            return Some(action);
        }
        self.message = None;
        self.confirm = Some((prompt, Confirmed::Run(action), "nothing written"));
        None
    }

    /// What came of writing `path`: with every conflict resolved, the
    /// offer to `git add` it; else how many kept their markers.
    pub(crate) fn resolved(
        &mut self,
        path: dffy_core::bstr::BString,
        result: Result<usize, String>,
    ) {
        let left = match result {
            Ok(left) => left,
            Err(why) => {
                self.failed(why);
                return;
            }
        };
        let files = self.doc.document.files();
        if let Some(file) = files.iter().position(|e| e.change().path == path) {
            self.doc.document.forget_choices(file);
            self.conflict_anchor = None;
        }
        let shown = escape(&path);
        if left > 0 {
            self.message(format!(
                "wrote {shown}; {} left with markers",
                plural(left, "conflict")
            ));
            return;
        }
        self.message = None;
        self.confirm = Some((
            format!("wrote {shown}; git add it to mark it resolved? y/n"),
            Confirmed::Run(Action::GitAdd(path)),
            "not added; git add marks it resolved",
        ));
    }

    /// After `file`'s diff was loaded: the view goes back to the conflict a
    /// choice was made on, and choices the file no longer fits, as it
    /// changed, are said to be gone. `chosen` is whether it had choices
    /// before.
    pub(super) fn conflict_loaded(&mut self, file: usize, chosen: bool) {
        if chosen && self.doc.document.files()[file].choices().is_none() {
            self.message("the file's conflicts changed: the choices made on it are dropped");
        }
        if let Some((at, region)) = self.conflict_anchor
            && at == file
            && self.doc.document.pos.file == file
        {
            self.conflict_anchor = None;
            self.doc.goto_conflict(region);
        }
    }

    /// The conflict a choice acts on: the one at `tapped`, else the one
    /// last gone to while it's on screen, else the first on screen.
    fn aimed_conflict(&mut self, tapped: Option<Pos>) -> Option<usize> {
        match tapped {
            Some(pos) => self.doc.conflicts_at(Some(pos)).first().copied(),
            None => self
                .current_conflict()
                .or_else(|| self.doc.conflicts_at(None).first().copied()),
        }
    }

    /// The choices for the conflict at `tapped`, else the one the keys act
    /// on, the one made reading as taking it back, then the write once
    /// there's a choice.
    pub(super) fn conflict_offers(&mut self, tapped: Option<Pos>) -> Vec<Offer> {
        let file = tapped.map_or(self.doc.document.pos.file, |p| p.file);
        let Ok(regions) = self.conflict_regions(file) else {
            return Vec::new();
        };
        let Some(region) = self.aimed_conflict(tapped) else {
            return Vec::new();
        };
        let choices = self.doc.document.files()[file].choices();
        let picked = choices.and_then(|c| c.picks.get(region).copied().flatten());
        let chosen = choices.is_some();
        let offer = |label: &str, action| Offer::new(label, action, Table::Conflict);
        let mut offers = Vec::new();
        for (choice, label, action) in [
            (Choice::Ours, "Keep current (ours)", Act::KeepCurrent),
            (Choice::Theirs, "Take incoming (theirs)", Act::TakeIncoming),
            (Choice::Both, "Keep both", Act::KeepBoth),
            (Choice::Base, "Take base", Act::TakeBase),
        ] {
            if regions[region].kept(choice).is_none() {
                continue;
            }
            let label = if picked == Some(choice) {
                format!("Undo: {label}")
            } else {
                label.to_owned()
            };
            offers.push(offer(&label, action));
        }
        if chosen {
            offers.push(offer("Write resolved file…", Act::WriteResolution));
        }
        offers
    }
}

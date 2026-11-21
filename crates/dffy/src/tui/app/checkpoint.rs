//! Checkpoints in the viewer: taking one, and the toggle between the diff
//! shown and what changed since the latest, which the status bar names
//! with its age. The core takes and compares them
//! ([`dffy_core::git::Repo::take_checkpoint`]); the loop takes one off the
//! main thread, by hand or as Fix with agent starts an agent.

use std::time::{Duration, Instant, SystemTime};

use dffy_core::git::{Checkpoint, Revisions, WhichCheckpoint};

use super::{Action, App};
use crate::keys::Action as Act;
use crate::review::model::text::ago;
use crate::tui::comments::ago_changes;

impl App {
    /// The key for `action`: the diff's, else the leader's after the
    /// leader key.
    pub(super) fn checkpoint_key(&self, action: Act) -> String {
        self.key_name(action)
            .or_else(|| {
                let (chord, _) = (self.keys.leader_entries().iter()).find(|(_, a)| *a == action)?;
                Some(format!("{} {}", self.key_name(Act::Leader)?, chord.name()))
            })
            .unwrap_or_else(|| action.name().to_owned())
    }

    /// "Since checkpoint": the latest checkpoint's tree against the
    /// working tree, or what showed before again.
    pub(super) fn toggle_since_checkpoint(&mut self) -> Option<Action> {
        if let Some((revisions, description)) = self.since_checkpoint.take() {
            self.description = description;
            return Some(self.show_revisions(revisions));
        }
        if self.checkpoint.is_none() {
            let take = self.checkpoint_key(Act::TakeCheckpoint);
            self.message(format!("no checkpoint yet; {take} takes one"));
            return None;
        }
        let Some((shown, _)) = &self.selected else {
            self.message("only a repository's diff has checkpoints");
            return None;
        };
        let before = (shown.clone(), self.description.clone());
        self.since_checkpoint = Some(before);
        "since checkpoint".clone_into(&mut self.description);
        Some(self.show_revisions(Revisions {
            since_checkpoint: Some(WhichCheckpoint::Latest),
            ..Revisions::default()
        }))
    }

    /// Lists `revisions` from now on, as what the diff selects.
    fn show_revisions(&mut self, revisions: Revisions) -> Action {
        if let Some((selected, _)) = &mut self.selected {
            selected.clone_from(&revisions);
        }
        self.dirty = true;
        Action::Retarget {
            revisions: Some(revisions),
            comments: None,
            moved: None,
        }
    }

    /// The latest checkpoint, read again as git's state changed.
    pub(super) fn checkpoint_read(&mut self, latest: Option<Checkpoint>) {
        self.checkpoint = latest;
        self.dirty = true;
    }

    /// A checkpoint was taken, or not. One taken while the diff shows the
    /// changes since the latest becomes what they're since.
    pub(super) fn checkpointed(
        &mut self,
        taken: Result<Checkpoint, String>,
        handoff: bool,
    ) -> Option<Action> {
        let checkpoint = match taken {
            Ok(checkpoint) => checkpoint,
            Err(why) => {
                self.failed(format!("couldn't take a checkpoint: {why}"));
                return None;
            }
        };
        self.checkpoint = Some(checkpoint);
        self.dirty = true;
        // The agent's start says how it went, after this.
        if !handoff {
            let since = self.checkpoint_key(Act::SinceCheckpoint);
            self.message(format!(
                "checkpoint {} taken · {since} shows what changes since",
                checkpoint.number
            ));
        }
        let (shown, _) = self.selected.as_ref()?;
        self.since_checkpoint.is_some().then(|| Action::Retarget {
            revisions: Some(shown.clone()),
            comments: None,
            moved: None,
        })
    }

    /// The status bar's words while the diff shows the changes since the
    /// latest checkpoint: `since checkpoint · 3m ago`.
    pub(super) fn checkpoint_chip(&self) -> Option<String> {
        self.since_checkpoint.as_ref()?;
        let Some(checkpoint) = self.shown_checkpoint() else {
            return Some(self.description.clone());
        };
        let then = u64::try_from(checkpoint.time).unwrap_or_default();
        let age = ago(now().saturating_sub(then)).unwrap_or_else(|| "a while ago".to_owned());
        Some(format!("{} · {age}", self.description))
    }

    /// The checkpoint the diff is since, when it's the latest, which is
    /// the one read.
    fn shown_checkpoint(&self) -> Option<Checkpoint> {
        let which = self.selected.as_ref()?.0.since_checkpoint?;
        self.checkpoint.filter(|c| match which {
            WhichCheckpoint::Latest => true,
            WhichCheckpoint::Number(n) => c.number == n,
        })
    }

    /// When the checkpoint's age in the status bar next reads differently.
    pub(super) fn checkpoint_deadline(&self) -> Option<Instant> {
        self.since_checkpoint.as_ref()?;
        let then = u64::try_from(self.shown_checkpoint()?.time).ok()?;
        let now = now();
        let next = ago_changes(then, now);
        (next != u64::MAX).then(|| Instant::now() + Duration::from_secs(next - now))
    }
}

/// Seconds since 1970.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

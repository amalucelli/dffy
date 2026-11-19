//! Your drafts and what posting them does: the comments a post sends, the
//! local review's drafts it moves into the session's file first, the
//! summary kept for it, and the words the review plugin's plan gives each
//! draft's heading.

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash as _, Hasher as _};

use dffy_core::bstr::BString;

use super::reload::Written;
use super::text::epoch;
use super::{Comments, Posting, Standing};
use crate::review::{self, Comment, Op};

impl Comments {
    /// Takes [`Comments::drafts_key`] as the drafts are now.
    pub(super) fn key_drafts(&mut self) {
        let mut drafts = std::hash::DefaultHasher::new();
        for draft in self.drafts() {
            draft.hash(&mut drafts);
        }
        self.drafts_key = drafts.finish();
    }

    /// The summary being written for the review, kept in the comments file.
    pub(crate) fn summary(&self) -> Option<&str> {
        self.review.summary.as_deref()
    }

    /// Writes the review's summary to the comments file, unless it's what
    /// the file has already; empty removes it. Returns what the read back
    /// found, as another writer's may have changed threads.
    pub(crate) fn keep_summary(&mut self, text: &str) -> Result<Written, String> {
        let summary = Some(text.trim_end()).filter(|s| !s.is_empty());
        if summary == self.summary() {
            return Ok(Written::default());
        }
        review::apply(&self.path, vec![Op::Summary(summary.map(str::to_owned))])?;
        Ok(self.reread())
    }

    /// The posted threads' roots, by `ref`: what a run importing from the
    /// host is compared with, before and after, to find the new ones.
    pub(crate) fn posted_roots(&self) -> HashMap<String, String> {
        self.review
            .comments
            .iter()
            .filter(|c| c.in_reply_to.is_none() && !c.path.is_empty())
            .filter_map(|c| Some((c.reference.clone()?, c.id.clone())))
            .collect()
    }

    /// How many of the comments shown are on the host for you only.
    pub(crate) fn host_pending(&self) -> usize {
        if let Some((generation, pending)) = self.pending_counted.get()
            && generation == self.generation
        {
            return pending;
        }
        let pending = self.review.comments.iter().filter(|c| c.pending).count();
        self.pending_counted.set(Some((self.generation, pending)));
        pending
    }

    /// The drafts posting sends, in file order, local ones in view
    /// included: each one's id, whether it's from the local review's file,
    /// and whether the host refused it before.
    pub(crate) fn drafts(&self) -> impl Iterator<Item = (&str, bool, bool)> {
        self.review
            .comments
            .iter()
            .filter(|c| self.standing(c) == Standing::Draft && !self.hides(c))
            .map(|c| {
                (
                    c.id.as_str(),
                    self.from_local.contains(&c.id),
                    c.post_error.is_some(),
                )
            })
    }

    /// [`Comments::drafts`] as of the last read, hashed: when it changes
    /// over a read, their plan is asked for again.
    pub(crate) fn drafts_key(&self) -> u64 {
        self.drafts_key
    }

    /// How many local drafts in view were written before `epoch` (seconds
    /// since 1970), or at a time that doesn't read as one.
    pub(crate) fn local_drafts_before(&self, epoch: u64) -> usize {
        self.local_shown()
            .filter(|c| self.standing(c) == Standing::Draft)
            .filter(|c| self::epoch(&c.created).is_none_or(|t| t < epoch))
            .count()
    }

    /// Takes what posting each draft does, by id. Returns the paths whose
    /// threads are to be placed again for it.
    pub(crate) fn set_plans(&mut self, plans: HashMap<String, Posting>) -> Vec<BString> {
        if plans == self.plans {
            return Vec::new();
        }
        let ids: HashSet<&String> = plans
            .iter()
            .filter(|&(id, word)| self.plans.get(id) != Some(word))
            .chain(self.plans.iter().filter(|(id, _)| !plans.contains_key(*id)))
            .map(|(id, _)| id)
            .collect();
        let mut paths: Vec<BString> = self
            .review
            .comments
            .iter()
            .filter(|c| ids.contains(&c.id))
            .map(|c| c.path.as_bytes().into())
            .collect();
        paths.sort_unstable();
        paths.dedup();
        self.plans = plans;
        self.generation += 1;
        self.conversation = OnceCell::new();
        self.remake(&paths);
        paths
    }

    /// Whether the host refused comment `id` when it was last posted.
    pub(crate) fn refused(&self, id: &str) -> bool {
        self.find(id).is_some_and(|c| c.post_error.is_some())
    }

    /// The local drafts in view, replies included: what posting moves.
    fn local_shown(&self) -> impl Iterator<Item = &Comment> {
        self.review
            .comments
            .iter()
            .filter(|c| self.from_local.contains(&c.id) && !self.hides(c))
    }

    /// How many drafts [`Comments::move_local`] would move.
    pub(crate) fn local_drafts(&self) -> usize {
        if self.from_local.is_empty() {
            return 0;
        }
        self.local_shown()
            .filter(|c| self.standing(c) == Standing::Draft)
            .count()
    }

    /// Moves the local drafts in view into the comments file, with new ids,
    /// and out of the local review's once that's written (see
    /// [`review::move_threads`]), then reads both again. Returns what the
    /// read back found.
    pub(crate) fn move_local(&mut self) -> Result<Written, String> {
        let roots = self.local_roots();
        let Some(local) = self.local.as_ref().filter(|_| !roots.is_empty()) else {
            return Ok(Written::default());
        };
        review::move_threads(local, &self.path, &roots)?;
        Ok(self.reread())
    }

    /// The first comments of the local drafts' threads in view.
    pub(crate) fn local_roots(&self) -> Vec<String> {
        self.local_shown()
            .filter(|c| c.in_reply_to.is_none())
            .map(|c| c.id.clone())
            .collect()
    }

    /// What posting draft `c` does, when that's news: the host refused it
    /// before, else what the last plan said.
    pub(super) fn posting(&self, c: &Comment) -> Option<Posting> {
        if self.standing(c) != Standing::Draft {
            return None;
        }
        match &c.post_error {
            Some(why) => Some(Posting {
                warn: true,
                words: format!("refused: {why}"),
            }),
            None => self.plans.get(&c.id).cloned(),
        }
    }
}

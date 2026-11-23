//! The tree's all-files mode: every file of the version at one side of
//! the diff, the diff's changes colored among them, made a folder at a
//! time from the version's cached list of files (see [`super::opening`]).
//! The changed-files tree waits meanwhile, kept current, and comes back
//! as it was; so does the all-files tree, its folds and place.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use dffy_core::bstr::BString;
use dffy_core::document::Side;

use super::{Action, App, Pane};
use crate::render::escape;
use crate::tui::picker::{self, Paths, SCORE_NOW};
use crate::tui::tree::{FILTER_SHOWN, Tree};
use crate::tui::worker::Job;

/// The tree filter's score of the version's files a worker is to do.
pub(super) struct TreeScore {
    id: u64,
    paths: Arc<Paths>,
    candidates: Arc<[usize]>,
    query: String,
}

/// The tree filter's last score of the version's files: its query, the
/// list it scored, and every match, best first, which a longer query
/// narrows.
pub(super) struct TreeLast {
    query: String,
    paths: Arc<Paths>,
    matches: Arc<[usize]>,
}

impl App {
    /// `a` in the tree: every file, or the diff's again; remembered. From
    /// the diff, the tree shows and takes the keys. Switching to all files
    /// reveals the file shown, once.
    pub(super) fn toggle_all_files(&mut self) {
        if let Some((_, changed)) = self.all_files.take() {
            // Its folds and place stay; its list of files goes.
            let mut parked = std::mem::replace(&mut self.tree, changed);
            parked.drop_list();
            self.all_parked = Some(parked);
            self.tree_score = None;
            self.tree_last = None;
            self.live_tree_score.store(u64::MAX, Ordering::Relaxed);
            self.release_listed();
            self.reveal = None;
            self.remember("all-files", "false".to_owned());
            self.rebuild_tree();
            self.message("the diff's files");
            return;
        }
        if let Err(why) = self.snapshot(Side::New) {
            self.message(format!("no other files: {why}"));
            return;
        }
        let shown = self.doc.document.files().get(self.doc.document.pos.file);
        self.reveal = shown
            .filter(|e| !e.is_commit())
            .map(|e| e.change().path.clone());
        self.remember("all-files", "true".to_owned());
        // Shown first, so the version's files are listed from the start.
        if !self.tree_shown {
            self.focus_tree();
        }
        self.enter_all_files(Side::New);
        self.pane = Pane::Tree;
        let back = (self.key_for(true, crate::keys::Action::ToggleAllFiles))
            .map(|k| format!(" · {k} back to the diff's files"))
            .unwrap_or_default();
        self.message(format!(
            "all files · {}{back}",
            self.version_name(Side::New)
        ));
    }

    /// Starts the tree on every file, as the config or the last run had it,
    /// where there are other files to list; they're listed once the tree
    /// shows.
    pub(crate) fn start_all_files(&mut self) {
        if self.snapshot(Side::New).is_ok() {
            self.enter_all_files(Side::New);
        }
    }

    /// The tree lists every file of the version at `side`, the
    /// changed-files tree put aside, the all-files tree left last back.
    pub(super) fn enter_all_files(&mut self, side: Side) {
        let parked = self.all_parked.take().unwrap_or_default();
        let changed = std::mem::replace(&mut self.tree, parked);
        self.all_files = Some((side, changed));
        self.build_all_files();
    }

    /// `s` in the all-files tree: the repository at the other version.
    pub(super) fn all_files_other_side(&mut self) {
        let Some((side, _)) = &mut self.all_files else {
            return;
        };
        *side = side.other();
        let side = *side;
        self.tree_last = None;
        self.build_all_files();
        self.release_listed();
        self.message(format!("all files · {}", self.version_name(side)));
    }

    /// Builds the all-files tree again from the version's files and the
    /// diff's, keeping its folds and place. The version's files are listed
    /// once the tree shows; until they come, the diff's alone show, under
    /// a title that says so.
    pub(super) fn build_all_files(&mut self) {
        let Some((side, _)) = self.all_files else {
            return;
        };
        let snapshot = self.snapshot(side).ok();
        let listed = snapshot
            .filter(|_| self.tree_shown)
            .and_then(|s| self.listed_or_list(s));
        let before = std::mem::take(&mut self.tree);
        // The list shown stays while the same version's is listed again,
        // never under another version's name.
        let kept = before.all_paths().filter(|&(_, at)| at == side);
        let listing = listed.is_none() && kept.is_none();
        let paths =
            (listed.or_else(|| kept.map(|(p, _)| p))).unwrap_or_else(|| Arc::new(Paths::default()));
        let version = self.version_name(side);
        let files = self.doc.document.files();
        self.tree = Tree::all_files(paths, files, (side, version, listing), Some(before));
        // A file outside the diff is there once the version's list is.
        if let Some(path) = &self.reveal
            && (self.tree.show_path(path) || !listing)
        {
            self.reveal = None;
        }
        if !self.tree.filter().is_empty() {
            self.score_tree_filter();
        }
        self.dirty = true;
    }

    /// The changed-files tree, which takes the diff's files as they're
    /// listed: the one shown, or the one put aside for all files.
    pub(super) fn changed_tree(&mut self) -> &mut Tree {
        match &mut self.all_files {
            Some((_, changed)) => changed,
            None => &mut self.tree,
        }
    }

    /// The all-files tree's filter or files changed: its matches scored
    /// now when there are few candidates, else by a worker, the matches
    /// shown staying meanwhile. A longer query scores the last one's
    /// matches alone. The diff's files the version lacks are scored too.
    pub(super) fn score_tree_filter(&mut self) {
        let Some((paths, _)) = self.tree.all_paths() else {
            return;
        };
        let query = self.tree.filter().to_owned();
        let id = picker::next_id();
        self.live_tree_score.store(id, Ordering::Relaxed);
        self.tree_score = None;
        if query.is_empty() {
            self.tree_last = None;
            return;
        }
        let jump = self.tree_last.as_ref().is_none_or(|l| l.query != query);
        let narrowed = (self.tree_last.as_ref())
            .filter(|l| Arc::ptr_eq(&l.paths, &paths) && query.starts_with(&l.query))
            .map(|l| Arc::clone(&l.matches));
        let candidates: Arc<[usize]> = narrowed.unwrap_or_else(|| (0..paths.len()).collect());
        if candidates.len() <= SCORE_NOW {
            let matches = paths
                .score(&query, &candidates, || false)
                .unwrap_or_default();
            self.tree_matches(query, &paths, &matches, jump);
        } else {
            self.tree.set_matched(None, false);
            self.tree_score = Some(TreeScore {
                id,
                paths,
                candidates,
                query,
            });
        }
    }

    /// Shows the filter's matches: the version's, `matches` of `paths`,
    /// with the diff's files the version lacks, best first.
    fn tree_matches(
        &mut self,
        query: String,
        paths: &Arc<Paths>,
        matches: &[(i64, usize)],
        jump: bool,
    ) {
        let extras = self.tree.all_extras();
        let mut scored: Vec<(i64, BString)> = (extras.into_iter())
            .filter_map(|path| {
                let mut label = escape(&path);
                label.make_ascii_lowercase();
                crate::fuzzy::rank(&query, &label).map(|s| (s, path))
            })
            .collect();
        let total = scored.len() + matches.len();
        scored.extend(
            (matches.iter())
                .take(FILTER_SHOWN)
                .map(|&(s, i)| (s, paths.path(i).to_owned())),
        );
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let shown = scored.into_iter().map(|(_, p)| p).collect();
        self.tree_last = Some(TreeLast {
            query,
            paths: Arc::clone(paths),
            matches: matches.iter().map(|&(_, i)| i).collect(),
        });
        self.tree.set_matched(Some((shown, total)), jump);
        self.dirty = true;
    }

    /// A worker's score `id`: the tree filter's when it waits for that
    /// one; else none of its own (see [`App::picker_scored`]). A score
    /// that was stopped is asked for again.
    pub(super) fn tree_scored(&mut self, id: u64, matches: &mut Option<Vec<(i64, usize)>>) -> bool {
        if self.tree_score.as_ref().is_none_or(|s| s.id != id) {
            return false;
        }
        if let (Some(matches), Some(score)) = (matches.take(), self.tree_score.take()) {
            let jump = self
                .tree_last
                .as_ref()
                .is_none_or(|l| l.query != score.query);
            self.tree_matches(score.query, &score.paths, &matches, jump);
        }
        true
    }

    /// The tree filter's score for a worker, while it waits for one.
    pub(super) fn tree_score_job(&self) -> Option<Job> {
        let s = self.tree_score.as_ref()?;
        Some(Job::Score {
            paths: Arc::clone(&s.paths),
            candidates: Arc::clone(&s.candidates),
            query: s.query.clone(),
            id: s.id,
            live: Arc::clone(&self.live_tree_score),
        })
    }

    /// Enter or a tap on a file the diff doesn't list, in the all-files
    /// tree: shown whole, at the version the tree shows.
    pub(super) fn open_all_path(&mut self, i: usize) -> Option<Action> {
        let (side, _) = self.all_files.as_ref()?;
        let side = *side;
        let path = self.tree.all_path(i)?;
        self.open_other(path, side)
    }
}

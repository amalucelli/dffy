//! Files the diff doesn't list, opened whole from the picker or the
//! control API at the diff's old or new side: listing a side's files for
//! the picker, opening one, closing it, and looking the opened ones up
//! again as the diff's sides move. A side's files never go into the document; an opened file
//! does, as an entry after the diff's ([`Source::Unchanged`]).
//!
//! Lookups need the repository, which the loop and the workers own: the
//! loop finds where a file is ([`Action::OpenOther`]) and hands it back
//! here ([`App::other_found`], [`App::relooked`]).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use dffy_core::bstr::{BStr, BString};
use dffy_core::document::{Side, Source};
use dffy_core::git::{FileChange, Snapshot};

use super::{Action, App, Pane};
use crate::render::escape;
use crate::tui::picker::{Others, Paths, Picker};
use crate::tui::tree::Item;
use crate::tui::worker::{Done, Job};

/// Why a piped diff offers no other files.
const PIPED: &str = "a piped diff has only its files";
/// Why two files or folders compared outside git offer none.
const COMPARED: &str = "compared paths have only their files";

/// Where a file opened outside the diff was found, as the loop looked it
/// up: `Ok(None)` when no file is there.
pub(crate) type Found = Result<Option<dffy_core::git::Side>, String>;

impl App {
    /// The diff's sides as snapshots, as last resolved; `None` for paths
    /// compared outside git. Lists of other snapshots are dropped, and a
    /// listing of one stops.
    pub(crate) fn set_snapshots(&mut self, snapshots: Option<[Snapshot; 2]>) {
        self.snapshots = snapshots;
        let current = |s: &Snapshot| snapshots.is_some_and(|both| both.contains(s));
        self.listed.retain(|s, _| current(s));
        if self.listing.is_some_and(|(s, _)| !current(&s)) {
            self.stop_listing();
            self.relist_picker();
        }
    }

    /// The diff is shown reversed (`-R`), which versions are named by.
    pub(crate) fn set_reverse(&mut self, reverse: bool) {
        self.reverse = reverse;
    }

    fn stop_listing(&mut self) {
        self.listing = None;
        self.live_list.store(u64::MAX, Ordering::Relaxed);
    }

    /// The open picker or the all-files tree waited on a listing that
    /// stopped, for a list now gone: it's listed again.
    fn relist_picker(&mut self) {
        if let Some((side, _)) = self.all_files
            && self.tree_shown
            && let Ok(snapshot) = self.snapshot(side)
        {
            self.listed_or_list(snapshot);
        }
        let Some(side) = (self.picker.as_ref())
            .filter(|p| p.listing())
            .and_then(Picker::side)
        else {
            return;
        };
        let others = self.others(side);
        if let Some(picker) = &mut self.picker {
            picker.set_others(others);
        }
        self.picked_query();
    }

    /// The diff's files were read whole again, the index too: the index's
    /// and the working tree's lists of files may have changed with it.
    pub(super) fn forget_listed(&mut self) {
        self.listed.retain(|s, _| matches!(s, Snapshot::Tree(_)));
        if self
            .listing
            .is_some_and(|(s, _)| !matches!(s, Snapshot::Tree(_)))
        {
            self.stop_listing();
            self.relist_picker();
        }
    }

    /// The snapshot `side` is taken from, or why there's none.
    pub(super) fn snapshot(&self, side: Side) -> Result<Snapshot, &'static str> {
        if self.piped {
            return Err(PIPED);
        }
        self.snapshots.map(|s| s[side.index()]).ok_or(COMPARED)
    }

    /// The files at `side` the diff doesn't list, for the picker: listed
    /// already, or being listed from now on.
    fn others(&mut self, side: Side) -> Others {
        let snapshot = match self.snapshot(side) {
            Ok(snapshot) => snapshot,
            Err(why) => return Others::Unavailable(why.to_owned()),
        };
        match self.listed_or_list(snapshot) {
            Some(paths) => Others::ready(side, paths, self.doc.document.files()),
            None => Others::Listing(side),
        }
    }

    /// The files at `snapshot` as listed already, or `None` while a
    /// worker lists them from now on.
    pub(super) fn listed_or_list(&mut self, snapshot: Snapshot) -> Option<Arc<Paths>> {
        if let Some(paths) = self.listed.get(&snapshot) {
            return Some(Arc::clone(paths));
        }
        if self.listing.is_none_or(|(s, _)| s != snapshot) {
            self.list_id += 1;
            self.listing = Some((snapshot, self.list_id));
            self.live_list.store(self.list_id, Ordering::Relaxed);
        }
        None
    }

    /// `o`: the picker, the other files at the new side under the diff's.
    pub(super) fn open_picker(&mut self) {
        self.doc.selection = None;
        let others = self.others(Side::New);
        self.picker = Some(Picker::new(self.doc.document.files(), others));
        self.picked_query();
    }

    /// The picker's `tab`: the other files at the other side.
    pub(super) fn picker_other_side(&mut self) {
        let Some(side) = self.picker.as_ref().and_then(Picker::side) else {
            return;
        };
        let others = self.others(side.other());
        if let Some(picker) = &mut self.picker {
            picker.set_others(others);
        }
        self.picked_query();
        self.release_listed();
    }

    /// Closes the picker, letting its list of other files go.
    pub(super) fn close_picker(&mut self) {
        self.picker = None;
        self.picked_query();
        self.release_listed();
    }

    /// Lets the lists of files go that neither the open picker nor the
    /// all-files tree shown use: a kernel's is several megabytes, and
    /// listing one again takes a worker tens of milliseconds.
    pub(super) fn release_listed(&mut self) {
        let side = |s: Option<Side>| s.and_then(|s| self.snapshot(s).ok());
        let in_use = [
            side(self.picker.as_ref().and_then(Picker::side)),
            side(self.all_files.as_ref().map(|&(s, _)| s)),
        ];
        self.listed.retain(|s, _| in_use.contains(&Some(*s)));
        if self
            .listing
            .is_some_and(|(s, _)| !in_use.contains(&Some(s)))
        {
            self.stop_listing();
        }
    }

    /// The picker's query changed, or it closed: a worker scoring the
    /// other files for another query stops.
    pub(super) fn picked_query(&self) {
        let id = self.picker.as_ref().map_or(u64::MAX, Picker::query_id);
        self.live_score.store(id, Ordering::Relaxed);
    }

    /// The files at `snapshot`, listed for listing `id`, or why not.
    pub(super) fn listed_paths(
        &mut self,
        snapshot: Snapshot,
        id: u64,
        result: Result<Arc<Paths>, Option<String>>,
    ) {
        if self.listing != Some((snapshot, id)) {
            return;
        }
        let result = match result {
            Ok(paths) => Ok(paths),
            Err(Some(why)) => Err(why),
            // Stopped: asked for again while it's still wanted.
            Err(None) => return,
        };
        self.stop_listing();
        let all = self.all_files.as_ref().map(|&(side, _)| side);
        let all = all.filter(|&s| self.snapshot(s) == Ok(snapshot));
        match &result {
            Ok(paths) => {
                self.listed.insert(snapshot, Arc::clone(paths));
            }
            // Said once; the tree shows the diff's files alone, not
            // listing again until the list is dropped.
            Err(why) if all.is_some() => {
                self.listed.insert(snapshot, Arc::new(Paths::default()));
                let version = all.map(|s| self.version_name(s)).unwrap_or_default();
                self.message(format!("listing the files in {version} failed: {why}"));
            }
            Err(_) => {}
        }
        if all.is_some() {
            self.build_all_files();
        }
        let side = self.picker.as_ref().and_then(Picker::side);
        let others = match result {
            Ok(paths) => side
                .filter(|&s| self.snapshot(s) == Ok(snapshot))
                .map(|s| Others::ready(s, paths, self.doc.document.files())),
            Err(why) => side.map(|s| {
                Others::Unavailable(format!(
                    "listing the files in {} failed: {why}",
                    self.version_name(s)
                ))
            }),
        };
        if let (Some(others), Some(picker)) = (others, &mut self.picker) {
            picker.set_others(others);
            self.picked_query();
            self.dirty = true;
        }
    }

    /// A worker's listing of a version's files, or its score of them for
    /// the all-files tree's filter or the picker.
    pub(super) fn opening_done(&mut self, done: Done) {
        match done {
            Done::Listed {
                snapshot,
                id,
                result,
            } => self.listed_paths(snapshot, id, result),
            Done::Scored { id, mut matches } => {
                if !self.tree_scored(id, &mut matches) {
                    self.picker_scored(id, matches);
                }
            }
            _ => {}
        }
    }

    /// A worker's score of the picker's other files for query `id`.
    pub(super) fn picker_scored(&mut self, id: u64, matches: Option<Vec<(i64, usize)>>) {
        if let (Some(picker), Some(matches)) = (&mut self.picker, matches) {
            picker.take_score(id, matches);
            self.dirty = true;
        }
    }

    /// The listing, the picker's score and the all-files tree filter's the
    /// workers are to do.
    pub(super) fn opening_jobs(&self) -> impl Iterator<Item = Job> {
        let list = self.listing.map(|(snapshot, id)| Job::ListPaths {
            snapshot,
            id,
            live: Arc::clone(&self.live_list),
        });
        let score = (self.picker.as_ref())
            .and_then(Picker::score_work)
            .map(|work| Job::Score {
                paths: work.paths,
                candidates: work.candidates,
                query: work.query,
                id: work.id,
                live: Arc::clone(&self.live_score),
            });
        list.into_iter().chain(score).chain(self.tree_score_job())
    }

    /// The diff's own entry of `path`: its file, or a renamed file's old
    /// path.
    pub(super) fn listed_entry(&self, path: &BStr) -> Option<usize> {
        self.doc.document.files().iter().position(|e| {
            !e.is_commit()
                && !e.is_unchanged()
                && (e.change().path == path || e.change().old_path() == path)
        })
    }

    /// Opens `path` from the picker's other files, at `side`: the diff's
    /// own entry when it lists the path, else the loop looks it up.
    pub(super) fn open_other(&mut self, path: BString, side: Side) -> Option<Action> {
        if let Some(file) = self.listed_entry(path.as_ref()) {
            self.open(file);
            return None;
        }
        // The picker leaves out only the diff's files read so far: this
        // may be one still to come, which would then be listed twice.
        if self.enumerating {
            self.message("still reading the diff's files");
            return None;
        }
        match self.snapshot(side) {
            Ok(at) => Some(Action::OpenOther { path, at, side }),
            Err(why) => {
                self.message(why);
                None
            }
        }
    }

    /// Where `path`, picked at `side` or asked for through the control
    /// API, was found: shown whole as an entry after the diff's, in place
    /// of one of it at the other side. Why it isn't goes to the control
    /// API's asker when there's one, else to the status line.
    pub(crate) fn other_found(&mut self, path: &BStr, side: Side, found: Found) {
        if let Some(file) = self.listed_entry(path) {
            self.open(file);
            self.control_found(path, side, Ok(file));
            return;
        }
        let shown = escape(path);
        let at = match found {
            Ok(Some(at)) => at,
            Ok(None) => {
                if !self.control_found(path, side, Err(None)) {
                    self.message(format!("{shown} isn't in {}", self.version_name(side)));
                }
                return;
            }
            Err(why) => {
                if !self.control_found(path, side, Err(Some(&why))) {
                    self.message(format!("couldn't open {shown}: {why}"));
                }
                return;
            }
        };
        let change = FileChange::unchanged(path.to_owned(), at);
        let open = self.unchanged_file(path);
        // The place left is taken before the entry changes: one opened
        // anew, or the same at its other version, is another file, and a
        // line the control API asks for is a jump in the file shown too.
        let same = self.opened.get(path) == Some(&side) && !self.control_wants_line(path, side);
        match open.filter(|_| same) {
            Some(file) => self.leave_for(file),
            None => self.leave(),
        }
        let file = match open {
            Some(file) if self.opened.get(path) == Some(&side) => file,
            Some(file) => {
                self.doc.document.replace_unchanged(file, change);
                file
            }
            None => self.doc.document.show_unchanged(change),
        };
        self.opened.insert(path.to_owned(), side);
        self.looked_at = self.snapshots;
        self.list_changed();
        self.show_entry(file);
        self.control_found(path, side, Ok(file));
    }

    /// The entry of `path` shown outside the diff.
    pub(super) fn unchanged_file(&self, path: &BStr) -> Option<usize> {
        (self.doc.document.files().iter()).position(|e| e.is_unchanged() && e.change().path == path)
    }

    /// The entries changed: the tree and the picker list them again.
    pub(super) fn list_changed(&mut self) {
        self.rebuild_tree();
        self.rebuild_picker();
        self.dirty = true;
    }

    /// The open picker lists the entries again from the start, under a
    /// new query id that a worker's score must live under.
    pub(super) fn rebuild_picker(&mut self) {
        if let Some(picker) = &mut self.picker {
            picker.rebuild(self.doc.document.files());
        }
        self.picked_query();
    }

    /// The files opened outside the diff that a new list may have moved,
    /// the side each is at and its snapshot now, for the loop to look up
    /// again: a working-tree file always, an index entry after a rescan
    /// (`rescanned`), a tree's file once its side is another tree.
    pub(crate) fn wanted_lookups(&mut self, rescanned: bool) -> Vec<(BString, Side, Snapshot)> {
        let before = std::mem::replace(&mut self.looked_at, self.snapshots);
        let moved = |side: Side, now: Snapshot| match now {
            Snapshot::Worktree => true,
            Snapshot::Index => rescanned,
            Snapshot::Tree(_) => before.is_none_or(|b| b[side.index()] != now),
        };
        (self.opened.iter())
            .filter_map(|(path, &side)| {
                let now = self.snapshot(side).ok()?;
                moved(side, now).then(|| (path.clone(), side, now))
            })
            .collect()
    }

    /// Where the files opened outside the diff are now, as the loop looked
    /// them up again after the list changed: one that moved is read
    /// again, one that's gone is closed and said, unless it has comments.
    pub(crate) fn relooked(&mut self, found: Vec<(BString, Side, Found)>) {
        let mut changed = false;
        let mut gone = Vec::new();
        for (path, side, found) in found {
            let Some(file) = self.unchanged_file(path.as_ref()) else {
                // The list has it now, or it was closed meanwhile.
                self.opened.remove(&path);
                continue;
            };
            match found {
                Ok(Some(at)) if self.doc.document.files()[file].change().new != Some(at) => {
                    let change = FileChange::unchanged(path.clone(), at);
                    changed |= self.doc.document.replace_unchanged(file, change);
                }
                Ok(None) if !self.has_comments(path.as_ref()) => {
                    self.close_entry(file);
                    self.opened.remove(&path);
                    gone.push(format!("{} ({})", escape(&path), self.version_name(side)));
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            self.list_changed();
        }
        if !gone.is_empty() {
            self.message(format!("closed, gone: {}", gone.join(", ")));
        }
    }

    fn has_comments(&self, path: &BStr) -> bool {
        (self.comments.as_ref()).is_some_and(|c| c.tally(path).is_some())
    }

    /// `x` in the tree: closes the file selected there, or elsewhere the
    /// file shown, when it was opened outside the diff.
    pub(super) fn close_file(&mut self) {
        let file = if self.pane == Pane::Tree {
            // The comments panel in the tree's place lists no files.
            let item = self.panel.is_none().then(|| self.tree.item()).flatten();
            // In the all-files tree, an opened file is a row of the
            // version's: its entry is found by path.
            let opened = |i| {
                let path = self.tree.all_path(i)?;
                self.unchanged_file(path.as_ref())
            };
            let file = match item {
                Some(Item::File(file)) => Some(file),
                Some(Item::Path(i)) => opened(i),
                _ => None,
            };
            let Some(file) = file else {
                self.message("select a file opened outside the diff to close");
                return;
            };
            file
        } else {
            self.doc.document.pos.file
        };
        let Some(entry) = self.doc.document.files().get(file) else {
            return;
        };
        if !entry.is_unchanged() {
            self.message("only files opened outside the diff close");
            return;
        }
        let path = entry.change().path.clone();
        if self.has_comments(path.as_ref()) {
            self.message("it has comments; it stays listed");
            return;
        }
        self.close_entry(file);
        self.opened.remove(&path);
        self.list_changed();
        self.message(format!("closed {}", escape(&path)));
    }

    /// Takes entry `file` away, and what the viewer held by its position.
    fn close_entry(&mut self, file: usize) {
        if !self.doc.document.close_unchanged(file) {
            return;
        }
        if let Some(search) = &mut self.search {
            if file < search.counts.len() {
                search.counts.remove(file);
            }
            search.pending = None;
            search.resume = None;
        }
        // Entries opened outside the diff come last: what held one of them
        // by its place goes.
        self.thread_pending.take_if(|(f, _)| *f >= file);
        if self.render.as_ref().is_some_and(|t| t.file >= file) {
            self.render = None;
        }
        self.tapped = None;
        self.press = None;
        self.anchor = None;
        self.doc.selection = None;
        self.doc.normalize();
        // The file shown may sit at another place now.
        self.sync_threads();
    }

    /// Whether entry `file` was opened at the old side, which the tree
    /// tags.
    pub(super) fn opened_at_old(&self, file: usize) -> bool {
        (self.doc.document.files().get(file))
            .filter(|e| matches!(e.source(), Source::Unchanged))
            .is_some_and(|e| self.opened.get(&e.change().path) == Some(&Side::Old))
    }
}

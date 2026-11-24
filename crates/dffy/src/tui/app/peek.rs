//! Go to definition: `K` lists the names on a line to pick one, a
//! ctrl+click or alt+click takes the name under the pointer, and a peek
//! under the line shows where it's defined in that line's version.
//!
//! The navigators and their index live in the navigator helper, a process
//! of its own (docs/navigate.md), which the first ask starts; asking it is
//! the pool's jobs, which wait on its answers. Each version of the diff
//! gets a navigator there, made on the first ask about it, whose index the
//! helper builds on a thread of its own, the version asked about last
//! first, pushing how far it is. A lookup answers from what's built so far
//! and says so, and is asked again once its version's index is complete.
//! A new ask cancels the one running. A helper that exits is started again
//! on the next ask, a new generation whose ids never meet the old one's.
//!
//! When the diff's list is read again, the navigator of the version the
//! peek shows is opened again at once and its lookup asked again; the
//! other's waits for its next ask. The helper keeps a navigator whose
//! files stayed and makes another for a version a refresh moved. Ten
//! minutes after the last peek closed, with no `K` since, the helper is
//! closed, which gives its memory back, and the next ask starts another
//! that reads the index back from the disk cache.

use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use dffy_core::annotation::Anchor;
use dffy_core::bstr::{BStr, BString};
use dffy_core::diff::Text;
use dffy_core::document::{Attach, Entry, Pos, Side, State};
use dffy_core::git::{self, Snapshot};
use dffy_core::highlight::Language;
use dffy_core::navigate::{self, Filter, Origin, Progress, Use};

use super::{Action, App, Pane};
use crate::keys::{Action as Act, Chord};
use crate::navigator::client::{Built, Cancel, Helper, Helpers, Opened, Pushed};
use crate::render::{escape, thousands};
use crate::term::{Input, Key, Mods};
use crate::tui::menu;
use crate::tui::peek::{Found, Names, Peek, Picked, Refs};
use crate::tui::view::{Doc, Hit};
use crate::tui::worker::{
    Code, HelperSlot, Job, Looked, LookupJob, NO_CACHE, NavigatorJob, PeekCodeJob, Referenced,
    ReferencesJob, Unanswered,
};

/// How long the helper is kept with no peek open and no `K`.
const IDLE: Duration = Duration::from_secs(10 * 60);

/// Searches for a name's uses whose answer is kept, by version and name.
const KEPT_REFERENCES: usize = 8;

/// A search for a name's uses: its version's snapshot, the asking file
/// (whose neighbours come first), the name and which uses it keeps.
type RefKey = (Snapshot, BString, String, Filter);

/// The search for the peek's name's uses, until it's done.
struct RefSearch {
    id: u64,
    key: RefKey,
    side: Side,
    origin: Origin,
    /// It goes on past its first files with a use: asked to.
    past_cap: bool,
    /// It stopped at its first files, until asked to go on.
    capped: bool,
    /// The generation of the helper its search runs in: one from a helper
    /// gone is started over.
    generation: u64,
    /// The helper's search, once its first job started it.
    search: Arc<Mutex<Option<u64>>>,
    cancel: Arc<Cancel>,
    /// The helper went away in its last step: not asked of a third.
    died: bool,
}

/// Files whose highlighted lines the peek keeps, for moving between
/// definitions without reading them again.
const CODES: usize = 4;

/// A navigator being opened by job `id` for snapshot `at`: the helper
/// keeps the one it has when the core says it still fits.
struct Making {
    id: u64,
    at: Snapshot,
}

/// A version's navigator, held by the helper.
#[derive(Default)]
struct Slot {
    /// The one answering lookups: the generation of the helper holding it,
    /// its id there, and the snapshot it lists. An id means nothing to a
    /// helper of another generation.
    ready: Option<(u64, u64, Snapshot)>,
    making: Option<Making>,
    /// The diff's list was read again since `ready` was opened: it's
    /// opened again before its next lookup.
    stale: bool,
    /// How far `ready`'s index is built, as the helper said last.
    built: Built,
    /// A tags query that doesn't compile was said for this navigator.
    said: bool,
}

impl Slot {
    /// The navigator to look up in, with its helper's generation: one not
    /// being opened again.
    fn answering(&self) -> Option<(u64, u64)> {
        self.ready
            .as_ref()
            .filter(|_| self.making.is_none())
            .map(|&(generation, id, _)| (generation, id))
    }

    fn holds(&self) -> bool {
        self.ready.is_some() || self.making.is_some()
    }
}

/// What a lookup asks from: the version it looks in, the file asking, and
/// where its peek hangs.
#[derive(Clone)]
struct Ask {
    side: Side,
    origin: Origin,
    /// The asking file's path in each version.
    paths: [BString; 2],
    attach: Attach,
    /// Where the name is used in the asking file: its line, and its column
    /// when the name was clicked. None in the other version, whose lines
    /// differ.
    at: Option<Use>,
}

/// A lookup asked for, until its answer comes.
struct Asked {
    id: u64,
    name: String,
    ask: Ask,
    /// The text search, after no definition was found.
    text: bool,
    cancel: Arc<Cancel>,
    /// The candidate to keep the cursor on, when it's asked again.
    keep: Option<(BString, usize)>,
    /// Asked again: the peek shows what it did until the answer.
    again: bool,
    /// The helper went away on it once: not asked of a third, which leaves
    /// a start for the next name.
    died: bool,
}

/// Where line `at`, whose text and neighbours `held` has, is in `text`, and
/// whether it's still the same line rather than one changed in place,
/// between the same neighbours. Its neighbours come before its old number:
/// an insertion can put an equal line there.
fn find_line(held: &Anchor, text: &Text, at: usize) -> Option<(usize, bool)> {
    let above = at.checked_sub(held.before.len())?;
    let find = |lines: Vec<String>, at| {
        let anchor = Anchor {
            lines,
            ..Anchor::default()
        };
        Some(anchor.place(text, at)?.start)
    };
    let whole = [&held.before[..], &held.lines, &held.after].concat();
    if let Some(start) = find(whole, above) {
        return Some((start + held.before.len(), true));
    }
    if let Some(found) = held.place(text, at) {
        return Some((found.start, true));
    }
    let line = match find(held.before.clone(), above) {
        Some(start) => start + held.before.len(),
        None if held.before.is_empty() => 0,
        None => return None,
    };
    let below = match find(held.after.clone(), line + 1) {
        Some(below) => below,
        None if held.after.is_empty() => text.len(),
        None => return None,
    };
    (below == line + 1).then_some((line, false))
}

/// The lookup the peek shows.
#[derive(Clone)]
struct Shown {
    name: String,
    ask: Ask,
    /// The path of the entry the peek hangs on.
    file: BString,
    /// Its lookup of definitions came from an index not complete yet: it's
    /// asked again once the index is.
    partial: bool,
    /// It shows the text search's lines.
    text: bool,
    /// The line it hangs under, with its neighbours, to find it again in
    /// the file's next diff.
    line: Anchor,
    /// Its file changed and the new diff isn't in: until it tells where the
    /// line went, the lookup waits.
    moving: bool,
}

/// A line to go to once its file's diff is loaded.
struct Land {
    path: BString,
    side: Side,
    line: usize,
}

/// Go to definition's state in the viewer.
pub(crate) struct Navigation {
    /// The navigator helper across its restarts, once the first ask
    /// started one.
    helper: HelperSlot,
    sides: [Slot; 2],
    next_id: u64,
    names: Option<(Names, Ask)>,
    asked: Option<Asked>,
    shown: Option<Shown>,
    code: Option<PeekCodeJob>,
    /// The files read for the peek, most recent last.
    codes: Vec<(BString, git::Side, Arc<Code>)>,
    /// The one search for uses the viewer runs.
    refs: Option<RefSearch>,
    /// Which uses the peek lists.
    filter: Filter,
    /// The uses found by searches that finished, most recent last.
    kept_refs: Vec<(RefKey, Refs)>,
    land: Option<Land>,
    /// The last `K`, peek or key in one.
    pub(super) used: Option<Instant>,
    /// The disk cache is on: the index lives in it.
    disk_cache: bool,
    /// How long the helper is kept with no peek open and no `K`.
    idle: Duration,
}

impl Default for Navigation {
    fn default() -> Self {
        Self {
            helper: Arc::new(Mutex::new(Helpers::default())),
            sides: [Slot::default(), Slot::default()],
            next_id: 0,
            names: None,
            asked: None,
            shown: None,
            code: None,
            codes: Vec::new(),
            refs: None,
            filter: Filter::default(),
            kept_refs: Vec::new(),
            land: None,
            used: None,
            disk_cache: true,
            // A test knob, as `DFFY_DAEMON_IDLE_SECS` is dffyd's: the pty
            // tests can't wait ten minutes for the idle drop.
            idle: (std::env::var("DFFY_PEEK_IDLE_SECS").ok())
                .and_then(|s| s.parse().ok())
                .map_or(IDLE, Duration::from_secs),
        }
    }
}

impl Drop for Navigation {
    fn drop(&mut self) {
        self.helper
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .close();
    }
}

impl Navigation {
    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// The list of names to pick from, while it's open.
    pub(crate) fn names(&self) -> Option<&Names> {
        self.names.as_ref().map(|(names, _)| names)
    }

    /// The navigator of `side` to ask, with the helper holding it: none
    /// while a worker starts a helper, which the frame doesn't wait for, or
    /// when the navigator is of a helper gone.
    fn asking(&self, side: Side) -> Option<(Helper, u64)> {
        let (generation, id) = self.sides[side.index()].answering()?;
        let (running, helper) = self.helper()?;
        (running == generation).then_some((helper, id))
    }

    /// The helper running and its generation; none while a worker starts
    /// one.
    pub(super) fn helper(&self) -> Option<(u64, Helper)> {
        self.helper.try_lock().ok()?.current()
    }

    fn stop_lookup(&mut self) {
        if let Some(asked) = self.asked.take() {
            asked.cancel.cancel();
        }
    }

    fn stop_references(&mut self) {
        if let Some(search) = self.refs.take() {
            search.cancel.cancel();
            self.close_search(&search);
        }
    }

    /// Has the helper drop `search`'s search, once one was started in the
    /// helper running.
    fn close_search(&self, search: &RefSearch) {
        let id = *search.search.lock().unwrap_or_else(PoisonError::into_inner);
        if let (Some(id), Some((generation, helper))) = (id, self.helper())
            && generation == search.generation
        {
            helper.close_search(id);
        }
    }

    /// Stops the search for uses, and forgets the answers kept.
    pub(super) fn forget_references(&mut self) {
        self.stop_references();
        self.kept_refs.clear();
    }
}

/// What the uses' group says of `filter`.
fn uses_words(filter: Filter) -> String {
    let mut words = Vec::new();
    if filter.calls_only {
        words.push("calls only");
    }
    if filter.in_diff {
        words.push("in this diff");
    }
    words.join(" · ")
}

/// The line the row at `pos` shows on `side`, else on the other side, with
/// the side it's on.
fn line_on(doc: &mut Doc, pos: Pos, side: Side) -> Option<(Pos, Side, usize)> {
    let lines = doc.row_lines(pos);
    match (lines[side.index()], lines[side.other().index()]) {
        (Some(line), _) => Some((pos, side, line)),
        (None, Some(line)) => Some((pos, side.other(), line)),
        (None, None) => None,
    }
}

/// How far an index not complete yet is, for the peek: paused when its
/// build doesn't run, as when the other version's builds instead.
fn partial(built: &Built) -> String {
    let paused = if built.building { "" } else { ", paused" };
    format!(
        "partial: {} of {} files indexed{paused}",
        thousands(built.progress.done),
        thousands(built.progress.total)
    )
}

/// What the peek says over text matches while the index isn't complete.
fn unindexed(progress: Progress) -> String {
    format!(
        "no definition indexed yet ({} of {} files)",
        thousands(progress.done),
        thousands(progress.total)
    )
}

impl App {
    /// The disk cache is on or off: go to definition keeps its index there.
    pub(crate) fn set_disk_cache(&mut self, on: bool) {
        self.navigation.disk_cache = on;
    }

    /// Why go to definition can't look names up in this view.
    fn cannot_navigate(&self) -> Option<String> {
        if self.piped {
            return Some("a piped diff has no repository to look names up in".to_owned());
        }
        if !self.navigation.disk_cache {
            return Some(NO_CACHE.to_owned());
        }
        self.snapshot(Side::New)
            .err()
            .map(|why| format!("go to definition needs git's versions of the files: {why}"))
    }

    /// What to ask from line `line` of `side` of entry `file`, and the
    /// line's text; says why not when there's nothing to ask.
    fn ask_from(&mut self, file: usize, side: Side, line: usize) -> Option<(Ask, Vec<u8>)> {
        if let Some(why) = self.cannot_navigate() {
            self.message(why);
            return None;
        }
        let entry = self.doc.document.files().get(file)?;
        if entry.is_commit() {
            self.message("a commit's text has no names to look up");
            return None;
        }
        let change = entry.change();
        let paths = [change.old_path().to_owned(), change.path.clone()];
        let path = &paths[side.index()];
        let Some(language) = crate::grammars().detect(path, b"").filter(|l| l.has_tags()) else {
            let shown = escape(path);
            self.message(format!("go to definition doesn't read {shown}'s language"));
            return None;
        };
        let Some(diff) = self.doc.document.diff_of(file) else {
            self.message("this file isn't diffed yet");
            return None;
        };
        let text = match side {
            Side::Old => &diff.old,
            Side::New => &diff.new,
        };
        if line >= text.len() {
            return None;
        }
        let language = language.of_text(path, text.bytes());
        let bytes = text.line(line).to_vec();
        let origin = Origin {
            path: path.clone(),
            language: language.clone(),
        };
        let attach = Attach::Line { side, line };
        // The line alone, until the use picked or clicked gives its column.
        let at = Some(Use { line, column: None });
        Some((
            Ask {
                side,
                origin,
                paths,
                attach,
                at,
            },
            bytes,
        ))
    }

    /// `K`: the names on the line last tapped, else the top line, to pick
    /// the one to look up.
    pub(super) fn go_to_definition(&mut self) {
        // From the tree, as `v` does: the peek's keys are the diff's.
        if self.pane == Pane::Tree {
            self.leave_panel();
        }
        self.navigation.used = Some(Instant::now());
        let found = if let Some((pos, side)) = self.take_tapped() {
            line_on(&mut self.doc, pos, side)
        } else {
            let file = self.doc.document.pos.file;
            let first = self.doc.document.reading_of(file).unwrap_or(Side::New);
            (0..self.doc.height).find_map(|y| {
                let pos = self.doc.row_at(y)?;
                line_on(&mut self.doc, pos, first)
            })
        };
        let Some((pos, side, line)) = found else {
            self.message("no line of code here to look names up on");
            return;
        };
        let Some((ask, text)) = self.ask_from(pos.file, side, line) else {
            return;
        };
        let names = navigate::names_at(&text, ask.origin.language.name());
        if names.is_empty() {
            self.message("no names on this line");
            return;
        }
        let y = (0..self.doc.height)
            .find(|&y| self.doc.row_at(y) == Some(Pos { offset: 0, ..pos }))
            .unwrap_or(0);
        let at = (self.content_x() + 2, y + 2);
        self.navigation.names = Some((Names::new(names, at), ask));
    }

    /// Input while the list of names is open.
    pub(super) fn names_input(&mut self, input: &Input) -> Option<Action> {
        let size = (self.screen.width(), self.screen.height());
        let (names, _) = self.navigation.names.as_mut()?;
        let picked = match input {
            Input::Key(key, mods) => names.key(*key, *mods),
            Input::Click { x, y } => names.tap((usize::from(*x), usize::from(*y)), size),
            Input::Paste(text) => {
                for c in text.lines().next().unwrap_or_default().chars() {
                    names.key(Key::Char(c), Mods::NONE);
                }
                Picked::Stay
            }
            _ => Picked::Stay,
        };
        match picked {
            Picked::Stay => {}
            Picked::Close => self.navigation.names = None,
            Picked::Name(name, column) => {
                let (_, mut ask) = self.navigation.names.take()?;
                // The use picked, where it is: the same name twice on a
                // line may be of two scopes.
                ask.at = ask.at.map(|at| Use {
                    column: Some(column),
                    ..at
                });
                self.look_up(name, ask);
            }
        }
        None
    }

    /// A ctrl+click or alt+click stays one only over the diff's code with
    /// no overlay open; anywhere else it's a plain click.
    pub(super) fn peek_or_click(&mut self, input: Input) -> Input {
        let Input::PeekClick { x, y } = input else {
            return input;
        };
        let overlay = self.confirm.is_some()
            || self.doc.writing().is_some()
            || self.menu.is_some()
            || self.navigation.names.is_some()
            || self.commands.is_some()
            || self.help.is_some()
            || self.picker.is_some()
            || self.reviews.is_some()
            || self.posting.is_some();
        let (cx, cy) = (usize::from(x), usize::from(y));
        if !overlay && !self.over_tree(cx) && self.code_at(cx, cy).is_some() {
            input
        } else {
            Input::Click { x, y }
        }
    }

    /// A ctrl+click or alt+click on the diff's cell `x`, `y`: the name
    /// there is looked up at once.
    pub(super) fn peek_click(&mut self, x: usize, y: usize) {
        let hit = (x.checked_sub(self.content_x()))
            .zip(y.checked_sub(1))
            .and_then(|(column, row)| self.doc.hit(row, column, None));
        let Some(Hit::Code {
            pos,
            side,
            line,
            bytes,
        }) = hit
        else {
            self.message("no name here to look up");
            return;
        };
        self.tapped = Some((pos, side));
        self.pane = Pane::Diff;
        let Some((mut ask, text)) = self.ask_from(pos.file, side, line) else {
            return;
        };
        let text = text.strip_suffix(b"\n").unwrap_or(&text);
        let Some((name, column)) = navigate::name_at(text, bytes.start).and_then(|range| {
            let name = std::str::from_utf8(&text[range.clone()]).ok()?;
            Some((name.to_owned(), range.start))
        }) else {
            self.message("no name here to look up");
            return;
        };
        ask.at = Some(Use {
            line,
            column: Some(column),
        });
        self.look_up(name, ask);
    }

    /// Peeks at `name` as asked from line `line` of `side` of the file
    /// shown, as picking it from `K`'s list does, for the benchmarks;
    /// whether it could be asked.
    pub(crate) fn peek_name(&mut self, name: &str, side: Side, line: usize) -> bool {
        let file = self.doc.document.pos.file;
        let Some((ask, _)) = self.ask_from(file, side, line) else {
            return false;
        };
        self.look_up(name.to_owned(), ask);
        true
    }

    /// The navigator helper running, once an ask started it, for the
    /// benchmarks and tests.
    pub(crate) fn peek_helper(&self) -> Option<Helper> {
        self.navigation.helper().map(|(_, helper)| helper)
    }

    /// The navigator `side` asks of: its helper's generation and its id
    /// there, for tests.
    #[cfg(test)]
    pub(crate) fn peek_navigator(&self, side: Side) -> Option<(u64, u64)> {
        self.navigation.sides[side.index()].answering()
    }

    /// The generation of the helper the search for uses was started for,
    /// for tests.
    #[cfg(test)]
    pub(crate) fn peek_search_generation(&self) -> Option<u64> {
        self.navigation.refs.as_ref().map(|r| r.generation)
    }

    /// Tags the search for uses with `generation`, as one a helper gone
    /// left, for tests.
    #[cfg(test)]
    pub(crate) fn tag_peek_search(&mut self, generation: u64) {
        if let Some(search) = self.navigation.refs.as_mut() {
            search.generation = generation;
        }
    }

    /// Opens `side`'s navigator again, as a refresh does, for tests.
    #[cfg(test)]
    pub(crate) fn reopen_peek_navigator(&mut self, side: Side) {
        self.make_navigator(side);
    }

    /// An index of a navigator asked of is being built, as the helper
    /// pushed it last, for tests that wait for it.
    #[cfg(test)]
    pub(crate) fn peek_building(&self) -> bool {
        (self.navigation.sides.iter()).any(|s| s.answering().is_some() && s.built.building)
    }

    /// The peek shows its candidate's code, or why there's none, or
    /// nothing to list: it waits on no job.
    pub(crate) fn peek_settled(&self) -> bool {
        self.doc.def_peek().is_some_and(|p| match p.found() {
            Found::Listed(_) => p.code.is_some(),
            Found::Nothing(_) => true,
            Found::Waiting(_) => false,
        })
    }

    /// Line `line` of `side` of entry `file`, with its neighbours.
    fn held_line(&self, file: usize, side: Side, line: usize) -> Option<Anchor> {
        let diff = self.doc.document.diff_of(file)?;
        let text = match side {
            Side::Old => &diff.old,
            Side::New => &diff.new,
        };
        (line < text.len()).then(|| Anchor::new(text, line..line + 1))
    }

    /// Looks `name` up from `ask`, in a peek under its line, in place of
    /// any peek or lookup before.
    fn look_up(&mut self, name: String, ask: Ask) {
        let (side, attach) = (ask.side, ask.attach);
        self.navigation.used = Some(Instant::now());
        self.navigation.stop_references();
        let waiting = self.ensure_navigator(side);
        self.ask(name.clone(), ask.clone(), None, false);
        let version = self.version_name(side);
        let peek = Peek::new(name.clone(), version, waiting, self.peek_hints(side));
        self.navigation.code = None;
        let file = self.doc.document.pos.file;
        let line = match attach {
            Attach::Line { side, line } => self.held_line(file, side, line),
            Attach::Start | Attach::End => None,
        };
        let entry = self.doc.document.files().get(file);
        self.navigation.shown = Some(Shown {
            name,
            ask,
            file: entry.map(|e| e.change().path.clone()).unwrap_or_default(),
            partial: false,
            text: false,
            line: line.unwrap_or_default(),
            moving: entry.is_some_and(Entry::is_stale),
        });
        self.doc.set_focus(None);
        self.sync_threads();
        self.doc.open_peek(peek, attach);
        // The uses fill in below the definitions as they're found, each
        // group drawn as it comes.
        self.start_uses();
        self.dirty = true;
        self.kept = false;
    }

    /// Sends the lookup of `name` from `ask` to the pool once its version's
    /// navigator answers, cancelling the one before.
    fn ask(&mut self, name: String, ask: Ask, keep: Option<(BString, usize)>, again: bool) {
        self.navigation.stop_lookup();
        let id = self.navigation.id();
        self.navigation.asked = Some(Asked {
            id,
            name,
            ask,
            text: false,
            cancel: Arc::new(Cancel::default()),
            keep,
            again,
            died: false,
        });
    }

    /// Asks the peek's lookup again, keeping its cursor's candidate.
    fn ask_again(&mut self) {
        let Some(shown) = self.navigation.shown.clone() else {
            return;
        };
        let keep = (self.doc.def_peek())
            .and_then(Peek::current)
            .map(|c| (c.path.clone(), c.start));
        self.ask(shown.name, shown.ask, keep, true);
    }

    /// Makes `side`'s navigator when there's none or a refresh left it
    /// behind; what the peek says while it waits for it.
    fn ensure_navigator(&mut self, side: Side) -> String {
        let version = self.version_name(side);
        let slot = &self.navigation.sides[side.index()];
        if slot.making.is_none() {
            match &slot.ready {
                Some(_) if !slot.stale => {
                    return if slot.built.progress.complete() {
                        "looking it up…".to_owned()
                    } else {
                        format!("looking it up… ({})", partial(&slot.built))
                    };
                }
                _ => self.make_navigator(side),
            }
        }
        if self.navigation.sides[side.index()].making.is_some() {
            format!("listing the files in {version}…")
        } else {
            format!("{version} has no files to look in")
        }
    }

    /// Asks for `side`'s navigator: the ready one when it still fits the
    /// diff as listed now, else a new one.
    fn make_navigator(&mut self, side: Side) {
        let Ok(at) = self.snapshot(side) else {
            return;
        };
        let id = self.navigation.id();
        let slot = &mut self.navigation.sides[side.index()];
        slot.making = Some(Making { id, at });
        slot.stale = false;
    }

    /// The pool's go-to-definition work that someone waits on and that's
    /// quick: making a navigator, a lookup of definitions. They come before
    /// the files'.
    pub(super) fn peek_jobs(&self) -> impl Iterator<Item = Job> {
        let nav = &self.navigation;
        let make = [Side::Old, Side::New].into_iter().filter_map(|side| {
            let slot = &nav.sides[side.index()];
            let making = slot.making.as_ref()?;
            Some(Job::Navigator(NavigatorJob {
                side,
                snapshots: self.snapshots?,
                helper: Arc::clone(&nav.helper),
                id: making.id,
            }))
        });
        let lookup = (nav.asked.as_ref())
            .filter(|a| !a.text)
            .and_then(|a| self.lookup_job(a));
        make.collect::<Vec<_>>().into_iter().chain(lookup)
    }

    /// The read of the peek's code: on screen once the peek shows, it
    /// comes right after the work on the files on screen.
    pub(super) fn peek_code_job(&self) -> Option<Job> {
        self.navigation.code.clone().map(Job::PeekCode)
    }

    /// The pool's go-to-definition work that reads many files: the text
    /// search and the search for uses. They come after the files'.
    pub(super) fn peek_late_jobs(&self) -> impl Iterator<Item = Job> {
        let nav = &self.navigation;
        let text = (nav.asked.as_ref())
            .filter(|a| a.text)
            .and_then(|a| self.lookup_job(a));
        // Stopped at its first files, a search waits to be asked to go on;
        // and it waits for the peek's first lookup, which shares the
        // helper's time with it, to answer.
        let first_lookup = nav.asked.as_ref().is_some_and(|a| !a.again && !a.text);
        let references = (nav.refs.as_ref())
            .filter(|s| !s.capped && !first_lookup)
            .and_then(|search| {
                let (helper, navigator) = nav.asking(search.side)?;
                let (generation, _) = nav.sides[search.side.index()].answering()?;
                // A search of a helper gone is started over first.
                (generation == search.generation).then(|| {
                    Job::References(ReferencesJob {
                        helper,
                        navigator,
                        search: Arc::clone(&search.search),
                        name: search.key.2.clone(),
                        origin: search.origin.clone(),
                        filter: search.key.3,
                        past_cap: search.past_cap,
                        id: search.id,
                        cancel: Arc::clone(&search.cancel),
                    })
                })
            });
        text.into_iter().chain(references)
    }

    fn lookup_job(&self, asked: &Asked) -> Option<Job> {
        if self.navigation.shown.as_ref().is_some_and(|s| s.moving) {
            return None;
        }
        let (helper, navigator) = self.navigation.asking(asked.ask.side)?;
        Some(Job::Lookup(LookupJob {
            helper,
            navigator,
            name: asked.name.clone(),
            origin: asked.ask.origin.clone(),
            at: asked.ask.at,
            text: asked.text,
            id: asked.id,
            cancel: Arc::clone(&asked.cancel),
        }))
    }

    /// Starts the search for uses in `side` over, in the navigator that
    /// replaced the one it searched: a search steps only with its own.
    fn restart_references(&mut self, side: Side) {
        let id = self.navigation.id();
        let generation = (self.navigation.sides[side.index()].answering()).map(|(g, _)| g);
        if let Some(search) = (self.navigation.refs.as_ref()).filter(|r| r.side == side) {
            search.cancel.cancel();
            self.navigation.close_search(search);
        }
        let Some(search) = (self.navigation.refs.as_mut()).filter(|r| r.side == side) else {
            return;
        };
        search.cancel = Arc::new(Cancel::default());
        search.search = Arc::new(Mutex::new(None));
        search.id = id;
        search.capped = false;
        search.generation = generation.unwrap_or_default();
        self.edit_listed(|p| p.edit_references(|r| *r = Refs::default()));
    }

    /// The navigator of `side` asked for as `id` came, with the generation
    /// of the helper holding it, or why there's none. A navigator of a new
    /// helper leaves the other version's, of the helper before, to be
    /// opened again, and a search of it to start over.
    pub(super) fn navigator_made(
        &mut self,
        side: Side,
        id: u64,
        result: Result<(u64, Opened), Unanswered>,
    ) {
        let slot = &self.navigation.sides[side.index()];
        if slot.making.as_ref().is_none_or(|m| m.id != id) {
            return;
        }
        let nav = &mut self.navigation;
        let slot = &mut nav.sides[side.index()];
        let Some(making) = slot.making.take() else {
            return;
        };
        let waiting = nav.asked.as_ref().is_some_and(|a| a.ask.side == side);
        let first_ask = nav.asked.as_ref().is_some_and(|a| !a.again);
        self.dirty = true;
        match result {
            Ok((generation, opened)) => {
                let kept = slot
                    .ready
                    .is_some_and(|(g, n, _)| g == generation && n == opened.navigator)
                    && opened.fit != "new";
                if !kept {
                    slot.said = false;
                    slot.built = Built::default();
                }
                slot.built.progress = opened.progress;
                // The helper builds the index of the navigator it opened
                // last, until it pushes otherwise.
                slot.built.building = !opened.progress.complete();
                slot.ready = Some((generation, opened.navigator, making.at));
                let other = &mut nav.sides[side.other().index()];
                if other.ready.is_some_and(|(g, ..)| g != generation) {
                    other.ready = None;
                    other.built = Built::default();
                }
                let stale_search = (nav.refs.as_ref()).is_some_and(|r| r.generation != generation);
                if !kept {
                    self.restart_references(side);
                }
                if stale_search {
                    let searched = self.navigation.refs.as_ref().map(|r| r.side);
                    if let Some(searched) = searched {
                        self.restart_references(searched);
                    }
                }
                if waiting && first_ask {
                    let words = self.ensure_navigator(side);
                    self.peek_says(Found::Waiting(words));
                }
            }
            Err(why) => {
                // Those of a helper gone are opened again on the next ask.
                let running = nav.helper().map(|(generation, _)| generation);
                for slot in &mut nav.sides {
                    if slot.ready.is_some_and(|(g, ..)| Some(g) != running) {
                        slot.ready = None;
                        slot.built = Built::default();
                    }
                }
                let version = self.version_name(side);
                let words = match why {
                    Unanswered::Unavailable(why) => why,
                    Unanswered::Failed(why) => {
                        format!("couldn't list the files in {version}: {why}")
                    }
                    Unanswered::Gone | Unanswered::Cancelled => {
                        format!("couldn't list the files in {version}: the helper went away")
                    }
                };
                if self
                    .navigation
                    .refs
                    .as_ref()
                    .is_some_and(|r| r.side == side)
                {
                    self.references_failed(words.clone());
                }
                if waiting {
                    self.navigation.stop_lookup();
                    self.peek_says(Found::Nothing(words));
                }
            }
        }
    }

    fn peek_says(&mut self, found: Found) {
        self.edit_listed(|p| p.set_found(found, 0));
        self.dirty = true;
    }

    /// The helper of generation `generation` pushed how far an index is
    /// built. Only a change does anything: the peek says how far it is, a
    /// tags query that doesn't compile is said once, and the peek's lookup
    /// is asked again once its version's index is complete.
    pub(super) fn index_built(&mut self, generation: u64, pushed: &Pushed) {
        let Some(side) = [Side::Old, Side::New].into_iter().find(|s| {
            self.navigation.sides[s.index()].answering() == Some((generation, pushed.navigator))
        }) else {
            return;
        };
        let slot = &mut self.navigation.sides[side.index()];
        if slot.built == pushed.built {
            return;
        }
        slot.built = pushed.built.clone();
        let error = (slot.built.error.clone()).or_else(|| slot.built.note.clone());
        let say = error.filter(|_| !std::mem::replace(&mut slot.said, true));
        if let Some(error) = say {
            self.failed(format!("go to definition: {error}"));
        }
        let Some(shown) = self.navigation.shown.clone() else {
            return;
        };
        if self.doc.def_peek().is_none() || !shown.partial || shown.ask.side != side {
            return;
        }
        let built = &self.navigation.sides[side.index()].built;
        let note = (!built.progress.complete()).then(|| {
            if shown.text {
                unindexed(built.progress)
            } else {
                partial(built)
            }
        });
        self.doc.edit_peek(|p| p.note = note);
        self.dirty = true;
        self.ask_again_when_indexed();
    }

    /// Asks the peek's lookup again, for definitions, when its answer came
    /// from an index not complete then and complete now.
    fn ask_again_when_indexed(&mut self) {
        let Some(shown) = &self.navigation.shown else {
            return;
        };
        let slot = &self.navigation.sides[shown.ask.side.index()];
        let complete = slot.answering().is_some() && slot.built.progress.complete();
        if shown.partial && complete && self.navigation.asked.is_none() {
            self.ask_again();
        }
    }

    /// The files of `side`'s index built so far that have no definitions
    /// in it, and why: too large, too slow to parse, unreadable.
    fn skipped(&self, side: Side) -> Option<String> {
        let slot = &self.navigation.sides[side.index()];
        slot.answering()?;
        let [too_large, timed_out, unreadable] = slot.built.skipped;
        let parts: Vec<String> = [
            (too_large, "over 512KB"),
            (timed_out, "too slow to parse"),
            (unreadable, "unreadable"),
        ]
        .into_iter()
        .filter(|&(n, _)| n > 0)
        .map(|(n, why)| format!("{} {why}", thousands(n)))
        .collect();
        (!parts.is_empty()).then(|| format!("files not indexed: {}", parts.join(", ")))
    }

    /// Lookup `id`'s answer.
    pub(super) fn looked(&mut self, id: u64, result: Result<Looked, Unanswered>) {
        let Some(asked) = self.navigation.asked.take_if(|a| a.id == id) else {
            return;
        };
        self.dirty = true;
        let name = asked.name.clone();
        let looked = match result {
            Err(Unanswered::Cancelled) => return,
            Err(Unanswered::Gone) => {
                // Its navigator, or the helper, went away: opened again,
                // and asked again once it is.
                let side = asked.ask.side;
                self.navigation.sides[side.index()].stale = true;
                if asked.died {
                    let words =
                        format!("looking up {name} failed: the navigator helper went away twice");
                    self.peek_says(Found::Nothing(words));
                    return;
                }
                self.ensure_navigator(side);
                self.navigation.asked = Some(Asked {
                    died: true,
                    ..asked
                });
                return;
            }
            Err(Unanswered::Failed(why) | Unanswered::Unavailable(why)) => {
                self.peek_says(Found::Nothing(format!("looking up {name} failed: {why}")));
                return;
            }
            Ok(looked) => looked,
        };
        let slot = &mut self.navigation.sides[asked.ask.side.index()];
        // A push of the index's end can come before an answer from before
        // it: the answer doesn't take the index back.
        if slot.answering().is_some() && looked.progress.done > slot.built.progress.done {
            slot.built.progress = looked.progress;
        }
        // Only a lookup of definitions says how complete the answer is; the
        // text search after one keeps what it said.
        let partial_now = !looked.progress.complete();
        if let Some(shown) = &mut self.navigation.shown {
            if !looked.text {
                shown.partial = partial_now;
            }
            shown.text = looked.text;
        }
        if looked.candidates.is_empty() {
            let progress = looked.progress;
            let mut indexed = if progress.complete() {
                format!("no definition of {name} found")
            } else {
                format!(
                    "no definition of {name} indexed yet ({} of {} files)",
                    thousands(progress.done),
                    thousands(progress.total)
                )
            };
            if let Some(skipped) = self.skipped(asked.ask.side) {
                indexed = format!("{indexed}; {skipped}");
            }
            if looked.text {
                let words = format!("{indexed}, and no line uses it as a whole word");
                self.peek_says(Found::Nothing(words));
                self.ask_again_when_indexed();
                return;
            }
            self.peek_says(Found::Waiting(format!("{indexed} · searching text…")));
            let id = self.navigation.id();
            self.navigation.asked = Some(Asked {
                id,
                text: true,
                cancel: Arc::new(Cancel::default()),
                ..asked
            });
            return;
        }
        let cursor = (asked.keep.as_ref())
            .and_then(|(path, line)| {
                (looked.candidates.iter()).position(|c| c.path == *path && c.start == *line)
            })
            .unwrap_or(0);
        let note = match (looked.text, partial_now) {
            (true, true) => Some(unindexed(looked.progress)),
            (true, false) => Some("no definition found".to_owned()),
            (false, true) => {
                let built = &self.navigation.sides[asked.ask.side.index()].built;
                Some(partial(&Built {
                    progress: looked.progress,
                    ..built.clone()
                }))
            }
            (false, false) => None,
        };
        // Asked from the definition itself, what's new is where it's used.
        let here = !looked.text
            && (looked.candidates.first()).is_some_and(|c| {
                c.path == asked.ask.origin.path && asked.ask.at.is_some_and(|at| at.line == c.start)
            });
        self.doc.edit_peek(|p| {
            p.set_found(Found::Listed(looked), cursor);
            p.note = note;
            if here {
                p.open_on_uses();
            }
        });
        self.read_code();
        self.ask_again_when_indexed();
    }

    /// Changes what the peek lists with `edit`, and reads the code of the
    /// candidate shown when that's another one now, or its code is gone.
    fn edit_listed(&mut self, edit: impl FnOnce(&mut Peek)) {
        let current = |doc: &Doc| {
            let c = doc.def_peek().and_then(Peek::current)?;
            Some((c.path.clone(), c.side, c.start))
        };
        let before = current(&self.doc);
        self.doc.edit_peek(edit);
        let unread = self.navigation.code.is_none()
            && (self.doc.def_peek()).is_some_and(|p| p.code.is_none());
        if unread || current(&self.doc) != before {
            self.read_code();
        }
    }

    /// Shows the code of the peek's candidate: kept from before, or asked
    /// for.
    fn read_code(&mut self) {
        let Some(c) = self.doc.def_peek().and_then(Peek::current) else {
            self.navigation.code = None;
            return;
        };
        let (path, side) = (c.path.clone(), c.side);
        let nav = &mut self.navigation;
        if let Some(at) = (nav.codes.iter()).position(|(p, s, _)| *p == path && *s == side) {
            let kept = nav.codes.remove(at);
            let code = Arc::clone(&kept.2);
            nav.codes.push(kept);
            nav.code = None;
            self.doc.edit_peek(|p| p.code = Some(Ok(code)));
            return;
        }
        let language = crate::grammars().detect(&path, b"").cloned();
        let id = nav.id();
        nav.code = Some(PeekCodeJob {
            path,
            side,
            language,
            id,
        });
        self.doc.edit_peek(|p| p.code = None);
    }

    /// The peek's code for request `id` came, or why there's none.
    pub(super) fn code_read(&mut self, id: u64, result: Result<Code, String>) {
        let Some(job) = self.navigation.code.take_if(|j| j.id == id) else {
            return;
        };
        let result = result.map(Arc::new);
        if let Ok(code) = &result {
            let codes = &mut self.navigation.codes;
            codes.push((job.path, job.side, Arc::clone(code)));
            if codes.len() > CODES {
                codes.remove(0);
            }
        }
        self.doc.edit_peek(|p| p.code = Some(result));
        self.dirty = true;
    }

    /// Whether the peek has the keys: it's focused, no thread is, and a row
    /// of it is on screen.
    pub(super) fn peek_focused(&mut self) -> bool {
        self.doc.def_peek().is_some_and(|p| p.focused)
            && self.doc.focused().is_none()
            && self.doc.peek_shown()
    }

    /// A key while the peek has the keys: `Break` with what it did when its
    /// table binds it. A peek on screen without the keys takes them back,
    /// unless a thread has them, for one of its own: moving through it,
    /// opening, the other version, the uses. Its filters' keys stay the
    /// diff's (`m` goes to the next thread there).
    pub(super) fn peek_key(&mut self, key: Key, mods: Mods) -> ControlFlow<Option<Action>> {
        let Some(action) = self.keys.peek_action(key, mods) else {
            return ControlFlow::Continue(());
        };
        if !self.peek_focused() {
            let own = matches!(
                action,
                Act::ScrollDown
                    | Act::ScrollUp
                    | Act::Open
                    | Act::ShowInDiff
                    | Act::OtherVersion
                    | Act::References
            );
            if !own || self.doc.focused().is_some() || !self.doc.peek_shown() {
                return ControlFlow::Continue(());
            }
            self.doc.edit_peek(|p| p.focused = true);
            self.dirty = true;
        }
        ControlFlow::Break(self.peek_act(action))
    }

    /// What `action` does to the peek.
    pub(super) fn peek_act(&mut self, action: Act) -> Option<Action> {
        if self.doc.def_peek().is_none() {
            let key = self.key_name(Act::GoToDefinition);
            self.message(match key {
                Some(key) => format!("no peek open; {key} opens one"),
                None => "no peek open".to_owned(),
            });
            return None;
        }
        self.navigation.used = Some(Instant::now());
        match action {
            Act::ScrollDown | Act::ScrollUp => {
                let by = if action == Act::ScrollDown { 1 } else { -1 };
                let mut moved = false;
                self.doc.edit_peek(|p| moved = p.step(by));
                if moved {
                    self.read_code();
                }
            }
            Act::Open => return self.open_definition(false),
            Act::ShowInDiff => return self.open_definition(true),
            Act::OtherVersion => self.other_version(),
            Act::References => self.jump_uses(),
            Act::MoreUses => self.more_uses(),
            Act::CallsOnly => self.filter_uses(|f| f.calls_only = !f.calls_only),
            Act::UsesInDiff => self.filter_uses(|f| f.in_diff = !f.in_diff),
            Act::ClosePeek => self.close_peek(),
            // Rebound in the peek's table to something else: as elsewhere.
            other => return self.act(other),
        }
        None
    }

    /// Closes the peek and stops its lookup.
    pub(super) fn close_peek(&mut self) {
        self.forget_peek();
        if self.doc.close_peek().is_some() {
            self.dirty = true;
            self.kept = false;
        }
    }

    /// Stops what the peek waited on.
    fn forget_peek(&mut self) {
        let nav = &mut self.navigation;
        nav.stop_lookup();
        nav.stop_references();
        nav.shown = None;
        nav.code = None;
        nav.used = Some(Instant::now());
    }

    /// After an event: when the view dropped the peek (its file isn't shown
    /// any more), nothing goes on for it.
    pub(super) fn peek_sync(&mut self) {
        if self.navigation.shown.is_some() && self.doc.def_peek().is_none() {
            self.forget_peek();
        }
    }

    /// Gives the keys back to the diff, the peek staying.
    pub(super) fn blur_peek(&mut self) {
        if self.doc.def_peek().is_some_and(|p| p.focused) {
            self.doc.edit_peek(|p| p.focused = false);
        }
    }

    /// A tap on the peek, on row `y` of the screen at column `x`: it takes
    /// the keys, and a candidate tapped is shown.
    pub(super) fn peek_tap(&mut self, x: usize, y: usize) {
        self.doc.set_focus(None);
        self.doc.edit_peek(|p| p.focused = true);
        let column = x.saturating_sub(self.content_x());
        if let Some(i) = y
            .checked_sub(1)
            .and_then(|row| self.doc.peek_hit(row, column))
        {
            let mut moved = false;
            self.doc.edit_peek(|p| moved = p.set_cursor(i));
            if moved {
                self.read_code();
            }
        }
    }

    /// `enter`: the definition's file at its line, its entry when the diff
    /// lists it, else the whole file; with `in_diff` (`d`), only the
    /// diff's entry.
    fn open_definition(&mut self, in_diff: bool) -> Option<Action> {
        let side = self.navigation.shown.as_ref()?.ask.side;
        let Some(c) = self.doc.def_peek().and_then(Peek::current).cloned() else {
            self.message("no definition to go to yet");
            return None;
        };
        let path = c.path;
        let listed = self.listed_entry(path.as_ref());
        if in_diff && listed.is_none() {
            let shown = escape(&path);
            let key = self.keys.peek_chords(Act::Open).next().map(Chord::name);
            self.message(match key {
                Some(key) => format!("this diff doesn't change {shown}; {key} opens it whole"),
                None => format!("this diff doesn't change {shown}"),
            });
            return None;
        }
        self.close_peek();
        self.navigation.land = Some(Land {
            path: path.clone(),
            side,
            line: c.start,
        });
        let opened = listed.or_else(|| {
            (self.unchanged_file(path.as_ref())).filter(|_| self.opened.get(&path) == Some(&side))
        });
        if let Some(file) = opened {
            self.leave();
            self.open(file);
            self.peek_settle();
            return None;
        }
        let action = self.open_other(path, side);
        if action.is_none() {
            self.navigation.land = None;
        }
        action
    }

    /// Starts the search for the peek's name's uses, with the filter on,
    /// unless it runs or its answer is kept from before.
    fn start_uses(&mut self) {
        let Some(shown) = self.navigation.shown.clone() else {
            return;
        };
        let side = shown.ask.side;
        let Ok(at) = self.snapshot(side) else {
            return;
        };
        let filter = self.navigation.filter;
        let words = uses_words(filter);
        let more = (self.keys.peek_chords(Act::MoreUses).next())
            .map_or_else(String::new, |k| format!("{} more", k.name()));
        self.doc.edit_peek(|p| {
            p.uses_words = words;
            p.more_key = more;
        });
        let key = (
            at,
            shown.ask.origin.path.clone(),
            shown.name.clone(),
            filter,
        );
        let nav = &mut self.navigation;
        let kept = (nav.kept_refs.iter())
            .find(|(k, _)| *k == key)
            .map(|(_, r)| r.clone());
        let running = nav.refs.as_ref().is_some_and(|r| r.key == key);
        if let Some(refs) = kept {
            nav.stop_references();
            self.edit_listed(|p| p.edit_references(|r| *r = refs));
        } else if !running {
            nav.stop_references();
            let id = nav.id();
            let generation = nav.sides[side.index()].answering().map(|(g, _)| g);
            nav.refs = Some(RefSearch {
                id,
                key,
                side,
                origin: shown.ask.origin,
                past_cap: false,
                capped: false,
                generation: generation.unwrap_or_default(),
                search: Arc::new(Mutex::new(None)),
                cancel: Arc::new(Cancel::default()),
                died: false,
            });
            self.edit_listed(|p| p.edit_references(|r| *r = Refs::default()));
            self.ensure_navigator(side);
        }
        self.dirty = true;
    }

    /// `r` in the peek: from the definitions to the uses, or back.
    fn jump_uses(&mut self) {
        let mut moved = false;
        self.doc.edit_peek(|p| moved = p.jump_group());
        if moved {
            self.read_code();
            self.dirty = true;
        } else {
            let on_use = self.doc.def_peek().is_some_and(Peek::on_a_use);
            self.message(if on_use {
                "no definition listed"
            } else {
                "no use found yet"
            });
        }
    }

    /// `m` in the peek: the search for uses goes on past its first files.
    fn more_uses(&mut self) {
        let Some(search) = self.navigation.refs.as_mut().filter(|s| s.capped) else {
            self.message("no more uses to search for");
            return;
        };
        search.past_cap = true;
        search.capped = false;
        self.doc
            .edit_peek(|p| p.edit_references(|r| r.capped = false));
        self.dirty = true;
    }

    /// `c` or `i` in the peek: the uses listed through another filter,
    /// searched again.
    fn filter_uses(&mut self, change: impl FnOnce(&mut Filter)) {
        change(&mut self.navigation.filter);
        self.start_uses();
    }

    /// Stops the search for uses, the peek saying why.
    fn references_failed(&mut self, why: String) {
        self.navigation.stop_references();
        self.edit_listed(|p| {
            p.edit_references(|r| {
                r.failed = Some(why);
                r.done = true;
            });
        });
    }

    /// A step of reference search `id` came, or why it stopped.
    pub(super) fn referenced(&mut self, id: u64, result: Result<Referenced, Unanswered>) {
        let Some(side) = (self.navigation.refs.as_ref())
            .filter(|r| r.id == id)
            .map(|r| r.side)
        else {
            return;
        };
        self.dirty = true;
        let step = match result {
            Err(Unanswered::Cancelled) => return,
            Err(Unanswered::Gone) => {
                // Its navigator, or the helper, went away: opened again,
                // and the search started over with it.
                self.navigation.sides[side.index()].stale = true;
                let died = (self.navigation.refs.as_mut())
                    .is_some_and(|r| std::mem::replace(&mut r.died, true));
                if died {
                    self.references_failed("the navigator helper went away twice".to_owned());
                    return;
                }
                self.restart_references(side);
                self.ensure_navigator(side);
                return;
            }
            Err(Unanswered::Failed(why) | Unanswered::Unavailable(why)) => {
                self.references_failed(why);
                return;
            }
            Ok(step) => step,
        };
        if let Some(search) = &mut self.navigation.refs {
            search.died = false;
        }
        // Stopped at its first files, it goes on only when asked.
        let capped = !step.done && step.files >= navigate::MAX_REFERENCE_FILES;
        if let Some(search) = self.navigation.refs.as_mut()
            && !search.past_cap
            && capped
        {
            search.capped = true;
        }
        let capped = capped && self.navigation.refs.as_ref().is_some_and(|s| s.capped);
        self.edit_listed(|p| {
            p.add_references(|r| {
                for hit in step.hits {
                    if hit.more > 0 {
                        r.more.push((r.hits.len(), hit.more));
                    }
                    r.hits.push(hit.candidate);
                    r.lines.push(Some(hit.text));
                }
                r.files = step.files;
                r.searched = step.searched;
                r.done = step.done;
                r.capped = capped;
                r.partial |= !step.indexed;
            });
        });
        if step.done
            && let Some(search) = self.navigation.refs.take()
            && let Some(refs) = self.doc.def_peek().and_then(Peek::references)
        {
            // Uses found while the index was partial may count definitions'
            // own lines: asked again, they're found again.
            if !refs.partial {
                let kept = &mut self.navigation.kept_refs;
                kept.push((search.key, refs.clone()));
                if kept.len() > KEPT_REFERENCES {
                    kept.remove(0);
                }
            }
        }
    }

    /// `s` in the peek: the name's definitions in the diff's other version.
    fn other_version(&mut self) {
        let Some(shown) = self.navigation.shown.clone() else {
            return;
        };
        let side = shown.ask.side.other();
        if let Err(why) = self.snapshot(side) {
            self.message(why);
            return;
        }
        let ask = Ask {
            side,
            origin: Origin {
                path: shown.ask.paths[side.index()].clone(),
                language: shown.ask.origin.language.clone(),
            },
            at: None,
            ..shown.ask
        };
        self.look_up(shown.name, ask);
        let version = self.version_name(side);
        self.message(format!("definitions in {version}"));
    }

    /// Goes to the line a definition was opened at, once its file's diff
    /// is in.
    pub(super) fn peek_settle(&mut self) {
        let Some(land) = &self.navigation.land else {
            return;
        };
        let path: &BStr = land.path.as_ref();
        let Some(file) = (self.listed_entry(path)).or_else(|| self.unchanged_file(path)) else {
            return;
        };
        if file != self.doc.document.pos.file {
            self.navigation.land = None;
            return;
        }
        let (side, line) = (land.side, land.line);
        match self.doc.document.files()[file].state() {
            State::Pending => return,
            State::Failed(_) => {}
            State::Loaded(_) => {
                if !self.doc.goto_line(file, side, line) {
                    let version = self.version_name(side);
                    self.message(format!("no line {} in {version} here", line + 1));
                }
                self.dirty = true;
                self.kept = false;
            }
        }
        self.navigation.land = None;
    }

    /// `file` has a new diff: a peek in it follows the line it hangs under,
    /// which a lookup waiting for that asks from; one changed in place asks
    /// by name alone, where the name is on it not known any more. The peek
    /// closes when the line is gone.
    pub(super) fn peek_diffed(&mut self, file: usize) {
        let Some(shown) = &mut self.navigation.shown else {
            return;
        };
        let Attach::Line { side, line } = shown.ask.attach else {
            return;
        };
        let here = self.doc.threads_file() == file
            && (self.doc.document.files().get(file)).is_some_and(|e| e.change().path == shown.file);
        if !here {
            return;
        }
        let found = self.doc.document.diff_of(file).and_then(|diff| {
            let text = match side {
                Side::Old => &diff.old,
                Side::New => &diff.new,
            };
            let (line, same) = find_line(&shown.line, text, line)?;
            Some((line, same, Anchor::new(text, line..line + 1)))
        });
        let Some((line, same, held)) = found else {
            self.close_peek();
            self.message("the peek's line is gone; the peek closed");
            return;
        };
        let attach = Attach::Line { side, line };
        let moved = std::mem::replace(&mut shown.ask.attach, attach) != attach;
        shown.ask.at = (shown.ask.at.filter(|_| same)).map(|at| Use { line, ..at });
        shown.line = held;
        if std::mem::take(&mut shown.moving)
            && let Some(asked) = &mut self.navigation.asked
        {
            asked.ask = shown.ask.clone();
        }
        if moved {
            self.doc.move_peek(attach);
        }
    }

    /// The diff's list was read again, after the document took it. A peek
    /// whose file another took the place of closes. The navigator of the
    /// version the peek shows is brought up to date now and its lookup
    /// asked again; the other's on its next ask.
    pub(super) fn navigators_refreshed(&mut self) {
        if let Some(shown) = &mut self.navigation.shown {
            let file = self.doc.threads_file();
            let entry = self.doc.document.files().get(file);
            if let Some(entry) = entry.filter(|e| e.change().path == shown.file) {
                shown.moving |= entry.is_stale();
            } else {
                self.close_peek();
                self.message("the peek's file moved in the list; the peek closed");
            }
        }
        // A working-tree file's lines may have changed under the same name,
        // and the uses found in a version that isn't a tree with them.
        (self.navigation.codes).retain(|(_, side, _)| !matches!(side, git::Side::Worktree { .. }));
        let tree = |key: &RefKey| matches!(key.0, Snapshot::Tree(_));
        (self.navigation.kept_refs).retain(|(key, _)| tree(key));
        // A search of a version that moved, or isn't a tree, reads files no
        // longer there.
        let moved = (self.navigation.refs.as_ref())
            .is_some_and(|r| !tree(&r.key) || self.snapshot(r.side).ok() != Some(r.key.0));
        if moved {
            self.navigation.stop_references();
            self.edit_listed(|p| p.edit_references(|r| *r = Refs::default()));
        }
        let shown = (self.navigation.shown.as_ref()).map(|s| s.ask.side);
        for side in [Side::Old, Side::New] {
            let slot = &self.navigation.sides[side.index()];
            if slot.making.is_none() && slot.ready.is_none() {
                continue;
            }
            if shown == Some(side) {
                self.make_navigator(side);
            } else {
                self.navigation.sides[side.index()].stale = true;
            }
        }
        if shown.is_some() {
            self.ask_again();
            self.start_uses();
        }
    }

    /// When the navigators go if nothing asks for them: a while after the
    /// last peek closed.
    pub(super) fn navigation_deadline(&self) -> Option<Instant> {
        let nav = &self.navigation;
        let idle = nav.shown.is_none() && nav.names.is_none() && nav.land.is_none();
        let holds = nav.sides.iter().any(Slot::holds);
        nav.used.filter(|_| idle && holds).map(|t| t + nav.idle)
    }

    /// Closes the helper, which gives back its memory, once the deadline
    /// passed. Closing waits on nothing; while a worker starts a helper,
    /// it's tried again on the next tick.
    pub(super) fn drop_idle_navigators(&mut self) {
        if self
            .navigation_deadline()
            .is_none_or(|at| Instant::now() < at)
        {
            return;
        }
        let nav = &mut self.navigation;
        let Ok(mut helpers) = nav.helper.try_lock() else {
            return;
        };
        helpers.close();
        drop(helpers);
        nav.stop_lookup();
        for slot in &mut nav.sides {
            *slot = Slot::default();
        }
        nav.codes.clear();
        nav.forget_references();
        nav.used = None;
    }

    /// The peek's keys, as its bottom border says them.
    fn peek_hints(&self, side: Side) -> String {
        let key = |action| self.keys.peek_chords(action).next().map(Chord::name);
        let mut parts = Vec::new();
        if let (Some(down), Some(up)) = (key(Act::ScrollDown), key(Act::ScrollUp)) {
            parts.push(format!("{down}/{up} move"));
        }
        if let Some(k) = key(Act::Open) {
            parts.push(format!("{k} open"));
        }
        if let Some(k) = key(Act::ShowInDiff) {
            parts.push(format!("{k} show in diff"));
        }
        if let Some(k) = key(Act::OtherVersion)
            && self.snapshot(side.other()).is_ok()
        {
            parts.push(format!("{k} {}", self.version_name(side.other())));
        }
        if let Some(k) = key(Act::References) {
            parts.push(format!("{k} definitions/uses"));
        }
        if let Some(k) = key(Act::CallsOnly) {
            parts.push(format!("{k} calls"));
        }
        if let Some(k) = key(Act::UsesInDiff) {
            parts.push(format!("{k} in diff"));
        }
        if let Some(k) = key(Act::ClosePeek) {
            parts.push(format!("{k} close"));
        }
        parts.join(" · ")
    }

    /// The right-click menu on the peek: what its keys do.
    pub(super) fn peek_menu(&self) -> Vec<menu::Entry> {
        let entry = |label: &str, action| menu::Entry {
            label: label.to_owned(),
            key: (self.keys.peek_chords(action).next())
                .map(Chord::name)
                .unwrap_or_default(),
            run: menu::Run::Peek(action),
        };
        let other = self.navigation.shown.as_ref().map(|s| s.ask.side.other());
        let mut entries = vec![
            entry("Open", Act::Open),
            entry("Show in diff", Act::ShowInDiff),
        ];
        if let Some(side) = other.filter(|&s| self.snapshot(s).is_ok()) {
            entries.push(entry(
                &format!("In {}", self.version_name(side)),
                Act::OtherVersion,
            ));
        }
        let uses = if self.doc.def_peek().is_some_and(Peek::on_a_use) {
            "Definitions"
        } else {
            "Uses"
        };
        entries.push(entry(uses, Act::References));
        entries.push(entry("Calls only", Act::CallsOnly));
        entries.push(entry("Uses in diff", Act::UsesInDiff));
        entries.push(entry("Close", Act::ClosePeek));
        entries
    }

    /// Whether go to definition is offered on entry `file`'s `side`, for
    /// the right-click menu.
    pub(super) fn navigation_offered(&self, file: usize, side: Side) -> bool {
        self.cannot_navigate().is_none()
            && (self.doc.document.files().get(file)).is_some_and(|e| {
                let change = e.change();
                let path = match side {
                    Side::Old => change.old_path(),
                    Side::New => change.path.as_ref(),
                };
                !e.is_commit()
                    && (crate::grammars().detect(path, b"")).is_some_and(Language::has_tags)
            })
    }
}

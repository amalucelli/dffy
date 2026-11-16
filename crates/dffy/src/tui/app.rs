//! The viewer's state and what each event does to it. Nothing here touches
//! the terminal, threads or processes: the loop in [`super::run`] owns
//! those and carries out the [`Action`]s returned here.
//!
//! Keys go to one of two panes, the diff or the file tree; `tab` switches.
//! On wide terminals the tree is a panel beside the diff, on narrow ones
//! (phones included) an overlay over it that closes once a file is picked.
//! Every action has a key that needs neither ctrl nor a symbol, since
//! phone keyboards put symbols a layer away; mouse clicks and taps are
//! shortcuts only. Keys map to actions through the config's bindings.
//!
//! While lines are selected (`v`), moving moves the selection's end
//! instead of the view, `s` picks the side copied, and any other action
//! ends the selection first.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dffy_core::bstr::BString;
use dffy_core::diff::Whitespace;
use dffy_core::git::{FileChange, FileDiff, Revisions, Status, Update};
use dffy_core::view::Attributes;
use dffy_core::watch::Notice;
use unicode_width::UnicodeWidthStr;

use super::chrome::{self, Chrome, Cut, Heading, Switcher, Title};
use super::commands::{self, Commands, Outcome};
use super::help::Help;
use super::picker::{self, Picker};
use super::piped;
use super::tree::{self, Item, Totals, Tree};
use super::view::{Doc, Hit, Marks, Matches, Needle, Pos, Side, Source, State};
use super::worker::{Counted, Done, Job, RenderJob};
use super::{Event, view};
use crate::config::{
    self, Background, Config, Context, DefaultDiff, Indicators, ModifiedLines, StatusBar, TreeMode,
    View,
};
use crate::daemon::client;
use crate::keys::{Action as Act, Keys};
use crate::plugin::{self, Program, Renderers};
use crate::print;
use crate::render::{Layout, escape, thousands};
use crate::term::{self, Input, Key, Mods, Screen};
use crate::theme::{self, Role, Style, Theme};

/// Diffs kept in memory besides the ones on screen.
const CACHE_BYTES: usize = 256 << 20;
/// Files diffed ahead of the last one on screen and behind the first.
const PREFETCH_AHEAD: usize = 4;
const PREFETCH_BEHIND: usize = 1;
const MESSAGE_TIME: Duration = Duration::from_secs(3);
const TIP_TIME: Duration = Duration::from_secs(8);
/// Choices are written to the state file once they've settled this long.
const REMEMBER_DELAY: Duration = Duration::from_millis(500);
const WHEEL_ROWS: isize = 3;
/// Terminals at least this wide show the tree as a panel beside the diff,
/// shown from the start; narrower ones show it over the diff when asked.
const SIDE_TREE_WIDTH: usize = 100;
/// How long after the background query a light reply still switches tints.
const BACKGROUND_REPLY: Duration = Duration::from_millis(200);
/// Columns one horizontal scroll moves the code.
const SCROLL_COLUMNS: usize = 8;
/// Columns one `+` or `-` widens or narrows the tree by, and the narrowest
/// tree and diff they leave.
const TREE_STEP: usize = 4;
const MIN_TREE: usize = 20;
const MIN_DIFF: usize = 40;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Quit,
    /// Open a repository-relative path at a new-side line.
    Edit {
        path: BString,
        line: usize,
    },
    /// Start or stop watching.
    Watch(bool),
    /// Put `text` on the clipboard, then say `note`.
    Copy {
        text: Vec<u8>,
        note: String,
    },
    /// Turn the terminal's mouse reports on or off.
    Mouse(bool),
    /// Diff again with these settings: the files were marked stale.
    Rediff {
        highlight: bool,
        whitespace: Whitespace,
    },
}

/// The left button, held down over the diff.
struct Press {
    hit: Hit,
    /// Where it went down.
    at: (usize, usize),
    /// A selection came of it (a drag, or a double or triple click), to
    /// copy when it's let go.
    selected: bool,
}

/// The diff search (`/` in the diff): the pattern, typed or confirmed, and
/// how many times each file has it. The workers find the matches, in the
/// file shown first, so typing never waits for a scan.
struct Search {
    needle: Arc<Needle>,
    /// Keys go to the pattern.
    editing: bool,
    /// Bumped with each pattern, so late results are dropped and scans of
    /// an old one stop (see [`App::live_search`]).
    id: u64,
    /// Matches per entry, once known.
    counts: Vec<Option<usize>>,
    /// Where to go once a file's matches arrive: `n`, `N` or `enter`
    /// came before them.
    pending: Option<(usize, Land)>,
}

/// Where a search lands once a file's matches arrive.
#[derive(Debug, Clone, Copy)]
enum Land {
    First,
    Last,
    /// As `n` (`forward`) or `N` would; with `here`, a match on the top
    /// row counts (`enter` after typing).
    Step {
        forward: bool,
        here: bool,
    },
}

/// Files whose search matches are counted per frame's jobs.
const SEARCH_AHEAD: usize = 32;

/// Clicks this close in time on one cell count as a double or triple click.
const MULTI_CLICK: Duration = Duration::from_millis(400);
/// How often a drag held past the top or bottom of the pane scrolls on.
const EDGE_SCROLL: Duration = Duration::from_millis(50);

/// Watch mode as the status bar shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Watch {
    Off,
    /// Asked for; changes are picked up once it's ready.
    Starting,
    Watching,
    /// The watch limit is used up; files are polled once a second.
    Polling,
}

/// A file to render: its line diff, by which renderer, at what width and
/// for which background.
struct RenderTarget {
    file: usize,
    diff: Arc<FileDiff>,
    program: Program,
    width: usize,
    light: bool,
}

impl RenderTarget {
    fn same(&self, other: &Self) -> bool {
        (self.file, self.width, self.light) == (other.file, other.width, other.light)
            && Arc::ptr_eq(&self.diff, &other.diff)
            && self.program == other.program
    }
}

/// Where keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Diff,
    Tree,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent toggles and flags, not a state machine"
)]
pub(crate) struct App {
    doc: Doc,
    tree: Tree,
    theme: Theme,
    screen: Screen,
    description: String,
    /// The layout asked for; split renders unified on narrow screens.
    view: View,
    /// `view` is what an earlier run remembered, not this run's choice: a
    /// remembered split shows unified on phones.
    view_remembered: bool,
    status_bar: StatusBar,
    split_min_width: usize,
    wrap: bool,
    tab_width: usize,
    /// Columns the code is scrolled right by, without wrapping.
    scroll: usize,
    indicators: Indicators,
    modified_lines: ModifiedLines,
    line_numbers: bool,
    hunk_headers: bool,
    copy_decorations: bool,
    highlight: bool,
    whitespace: Whitespace,
    /// Lines of context around changes when files aren't whole, which
    /// `X` remembers as the `context` it goes back to.
    context_lines: usize,
    /// The theme's name, to reload it when the tints change.
    theme_name: String,
    /// Every theme by name, untinted, loaded on first need: stepping
    /// through themes and trying them in the theme list read no files.
    /// Opening the list loads them again, for theme files changed since.
    themes: Vec<(String, Theme)>,
    /// The terminal's background, as it said or as assumed; the tints
    /// mix into it.
    background: theme::Rgb,
    /// Color is on: themes and tints are worth switching.
    color: bool,
    /// Choices made here are remembered for the next run.
    remember: bool,
    /// Choices to remember, and when the last was made.
    remembered: Vec<(&'static str, String)>,
    remembered_at: Option<Instant>,
    commands: Option<Commands>,
    search: Option<Search>,
    /// The last search id handed out. Kept across cleared searches, so a
    /// scan for one that's gone never reports as the next one's.
    search_id: u64,
    /// The id of the search being typed or shown, `u64::MAX` with none;
    /// workers scanning for another stop.
    live_search: Arc<AtomicU64>,
    /// Renderer plugins by file extension, from the config. Empty, nothing
    /// about rendering runs.
    renderers: Renderers,
    /// The renderers looked up on PATH so far, found or not: each name is
    /// looked up once a session, when a file of its type first wants it.
    programs: Vec<(String, Option<Program>)>,
    /// The rendering the workers should be doing, if any.
    render: Option<RenderTarget>,
    /// Bumped whenever `render` changes.
    render_id: u64,
    /// `render_id` while `render` is set, `u64::MAX` otherwise; renderers
    /// running for another id are killed.
    live_render: Arc<AtomicU64>,
    /// The pane width renderings were last asked for.
    render_width: usize,
    keys: Keys,
    /// The tree's width as `+` and `-` set it; `None` follows the screen.
    tree_width: Option<usize>,
    /// The code row last clicked or tapped, where `v` starts.
    tapped: Option<(Pos, Side)>,
    press: Option<Press>,
    /// The last click: when, where, and how many in a row.
    clicks: Option<(Instant, (usize, usize), u8)>,
    /// A drag held past the top or bottom of the pane, where it went.
    drag_edge: Option<(usize, usize)>,
    /// The terminal sends mouse reports.
    mouse: bool,
    /// Tints are on and the config leaves dark or light to the terminal.
    ask_background: bool,
    /// When the background query went out, until its reply.
    asked: Option<Instant>,
    /// The tree is asked for (`T`, `tab`).
    tree_shown: bool,
    pane: Pane,
    /// The count pass should be (re)started for the current list.
    count_pass: bool,
    help: Option<Help>,
    picker: Option<Picker>,
    /// The narrow title's row and the columns of its previous-file arrow,
    /// file name and next-file arrow, as last drawn.
    switcher: Option<(usize, [Range<usize>; 3])>,
    /// The tree's `«` (on its border) or the diff's `»`, as last drawn.
    tree_button: Option<(usize, usize)>,
    /// The status bar's note and when it goes away.
    message: Option<(String, Instant)>,
    /// The note is the first-run tip, which any key dismisses.
    tip: bool,
    enumerating: bool,
    /// What the repository's diff selects, to word an empty list.
    selected: Option<(Revisions, DefaultDiff)>,
    /// What an empty `--staged` or `--unstaged` list leaves out.
    others: Option<print::Others>,
    /// The files come from a piped diff, in its order.
    piped: bool,
    watch: Watch,
    /// The `--focus` path to start at, until a file under it arrives or
    /// the user moves.
    focus: Option<BString>,
    frame: u64,
    visible: Range<usize>,
    dirty: bool,
}

impl App {
    /// A viewer showing `first`, or nothing yet in watch mode. `indicators`
    /// is the config's, already resolved for color.
    pub(crate) fn new(
        config: &Config,
        theme: Theme,
        description: String,
        first: Option<FileChange>,
        (width, height): (usize, usize),
        indicators: Indicators,
    ) -> Self {
        let placeholder = Layout {
            split: false,
            width: Some(width),
            wrap: config.wrap,
            words: false,
            tab_width: config.tab_width,
            indicators,
            one_sided: false,
            digits: 0,
            inline_signs: false,
            scroll: 0,
            line_numbers: config.line_numbers,
            hunk_headers: config.hunk_headers,
            modified_lines: config.modified_lines == ModifiedLines::Changed,
        };
        let mut doc = Doc::new(placeholder, height);
        doc.structural = config.syntax_diff;
        doc.rendered = config.rendered;
        doc.full = config.context == Context::Full;
        doc.overview = true;
        let mut tree = Tree::default();
        if let Some(first) = first {
            tree.insert(0, &first.path);
            doc.push(first, Source::Repo);
        }
        let mut app = Self {
            doc,
            tree,
            theme,
            screen: Screen::new(width, height),
            description,
            view: config.view,
            view_remembered: false,
            status_bar: config.status_bar,
            split_min_width: config.split_min_width,
            wrap: config.wrap,
            tab_width: config.tab_width,
            scroll: 0,
            indicators,
            modified_lines: config.modified_lines,
            line_numbers: config.line_numbers,
            hunk_headers: config.hunk_headers,
            copy_decorations: config.copy_decorations,
            highlight: config.highlight,
            whitespace: config.whitespace,
            context_lines: config.context.hunk_lines(true),
            theme_name: config.theme.clone(),
            themes: Vec::new(),
            background: if config.background == Background::Light {
                theme::LIGHT_BACKGROUND
            } else {
                theme::DARK_BACKGROUND
            },
            color: true,
            remember: config.remember,
            remembered: Vec::new(),
            remembered_at: None,
            commands: None,
            search: None,
            search_id: 0,
            live_search: Arc::new(AtomicU64::new(u64::MAX)),
            renderers: config.render.clone(),
            programs: Vec::new(),
            render: None,
            render_id: 0,
            live_render: Arc::new(AtomicU64::new(u64::MAX)),
            render_width: 0,
            keys: config.keys.clone(),
            tree_width: config.tree_width,
            tapped: None,
            press: None,
            clicks: None,
            drag_edge: None,
            mouse: config.mouse,
            ask_background: indicators == Indicators::Background
                && config.background == Background::Auto,
            asked: None,
            // On a narrow screen it covers the diff, so only `show` opens
            // it there; see `App::start_tree`.
            tree_shown: config.tree == TreeMode::Show
                || (config.tree == TreeMode::Auto && width >= SIDE_TREE_WIDTH),
            pane: Pane::Diff,
            count_pass: false,
            help: None,
            picker: None,
            switcher: None,
            tree_button: None,
            message: None,
            tip: false,
            enumerating: true,
            selected: None,
            others: None,
            piped: false,
            watch: Watch::Off,
            focus: None,
            frame: 0,
            visible: 0..1,
            dirty: true,
        };
        app.relayout();
        app
    }

    /// Starts at the first file under `path` once it arrives.
    pub(crate) fn focus(&mut self, path: BString) {
        self.focus = Some(path);
        self.check_focus();
    }

    /// Jumps to the newest file if it's under the focus path.
    fn check_focus(&mut self) {
        let (Some(focus), Some(file)) = (&self.focus, self.doc.files.len().checked_sub(1)) else {
            return;
        };
        let path = &self.doc.files[file].change.path;
        if focus.is_empty()
            || path == focus
            || (path.starts_with(&focus[..]) && path.get(focus.len()) == Some(&b'/'))
        {
            self.doc.goto_file(file);
            self.focus = None;
        }
    }

    /// The version of the entry at `file` (see [`view::Entry::version`]).
    pub(crate) fn version(&self, file: usize) -> u64 {
        self.doc.files.get(file).map_or(u64::MAX, |e| e.version)
    }

    /// What the repository's diff selects, and the default it was picked
    /// with, for the words of an empty list.
    pub(crate) fn set_selected(&mut self, revisions: Revisions, default: DefaultDiff) {
        self.selected = Some((revisions, default));
    }

    /// The files come from a piped diff: they keep its order.
    pub(crate) fn set_piped(&mut self) {
        self.piped = true;
    }

    /// With color off the theme is attributes only, and stays so.
    pub(crate) fn set_color(&mut self, color: bool) {
        self.color = color;
    }

    /// Shows the first-run tip until a key or about 8 seconds pass.
    pub(crate) fn show_tip(&mut self) {
        let leader = self
            .key_name(Act::Leader)
            .unwrap_or_else(|| "space".to_owned());
        let help = self.key_name(Act::Help).unwrap_or_else(|| "?".to_owned());
        self.message = Some((
            format!("tip: {leader} opens commands · {help} lists every key"),
            Instant::now() + TIP_TIME,
        ));
        self.tip = true;
        self.dirty = true;
    }

    /// The first key bound to `action` in the diff's table.
    fn key_name(&self, action: Act) -> Option<String> {
        self.keys
            .chords(false, action)
            .next()
            .map(crate::keys::Chord::name)
    }

    /// Notes a choice made here, to remember for the next run.
    fn remember(&mut self, key: &'static str, value: impl Into<String>) {
        if !self.remember {
            return;
        }
        let value = value.into();
        match self.remembered.iter_mut().find(|(k, _)| *k == key) {
            Some(pair) => pair.1 = value,
            None => self.remembered.push((key, value)),
        }
        self.remembered_at = Some(Instant::now());
    }

    /// The choices to write to the state file: once they've settled for a
    /// moment, or all of them with `now` (at exit).
    pub(crate) fn take_remembered(&mut self, now: bool) -> Vec<(&'static str, String)> {
        let settled = self
            .remembered_at
            .is_some_and(|at| at.elapsed() >= REMEMBER_DELAY);
        if now || settled {
            self.remembered_at = None;
            std::mem::take(&mut self.remembered)
        } else {
            Vec::new()
        }
    }

    /// The choices noted and not yet taken.
    pub(crate) fn pending_remembered(&self) -> &[(&'static str, String)] {
        &self.remembered
    }

    pub(crate) fn set_context_lines(&mut self, lines: usize) {
        self.context_lines = lines;
    }

    /// Entries of a piped diff to have read: a window past the one shown,
    /// or all of them while the picker, the tree's filter or a search
    /// looks through every file.
    pub(crate) fn wanted(&self) -> usize {
        if self.picker.is_some() || self.tree.editing || self.search.is_some() {
            self.doc.files.len() + piped::READ_ALL_AHEAD
        } else {
            self.doc.pos.file + piped::READ_AHEAD
        }
    }

    /// Marks watching as asked for, or as off when it can't be had.
    pub(crate) fn set_watching(&mut self, on: bool) {
        self.watch = if on { Watch::Starting } else { Watch::Off };
        // A watch that couldn't start isn't one to start next time.
        if !on {
            self.remembered.retain(|(k, _)| *k != "watch");
        }
        self.dirty = true;
    }

    /// Whether the tree is a panel beside the diff rather than over it.
    fn side_tree(&self) -> bool {
        self.screen.width() >= SIDE_TREE_WIDTH
    }

    /// The tree panel's width when it's beside the diff.
    fn side_width(&self) -> Option<usize> {
        let total = self.screen.width();
        (self.tree_shown && self.side_tree()).then(|| {
            self.tree_width
                .unwrap_or_else(|| tree::width(total))
                .min(total.saturating_sub(MIN_DIFF))
        })
    }

    /// `+` and `-` in the tree: wider or narrower by a step, within limits
    /// that leave both panes usable.
    fn resize_tree(&mut self, wider: bool) {
        let Some(width) = self.side_width() else {
            self.message("the tree is as wide as the screen here");
            return;
        };
        let max = self.screen.width().saturating_sub(MIN_DIFF).max(MIN_TREE);
        let width = if wider {
            width + TREE_STEP
        } else {
            width.saturating_sub(TREE_STEP)
        };
        let width = width.clamp(MIN_TREE, max);
        self.tree_width = Some(width);
        self.remember("tree-width", width.to_string());
        self.relayout();
    }

    /// Whether the screen has room for the frame (see [`chrome`]). Phones lose
    /// it even when tall: three columns of border cost more there than they give.
    fn framed(&self) -> bool {
        self.screen.height() >= chrome::FRAME_MIN_ROWS && self.screen.width() >= SIDE_TREE_WIDTH
    }

    /// The first column of code: a cell after the tree beside the diff,
    /// whose right border is the frame's divider, or after the frame's left
    /// border.
    fn content_x(&self) -> usize {
        match self.side_width() {
            Some(tree) => tree + 1,
            None if self.framed() => 2,
            None => 0,
        }
    }

    /// The tree's first row and its rows, borders included: the frame's
    /// whole height, or between the title and status rows without one.
    fn tree_rows(&self) -> (usize, usize) {
        let height = self.screen.height();
        if self.framed() {
            (0, height)
        } else {
            (1, height.saturating_sub(2))
        }
    }

    /// The first row the picker covers: it leaves the status row.
    fn picker_top(&self) -> usize {
        usize::from(self.status_bar == StatusBar::Top)
    }

    /// The layout came from the state file, not from this run's flags or
    /// the config.
    pub(crate) fn set_view_remembered(&mut self, remembered: bool) {
        self.view_remembered = remembered;
        self.relayout();
    }

    /// Settles a tree that starts over the diff on a narrow screen: it
    /// takes the keys, as after `t`, unless its visibility came from the
    /// state file. `t` remembers it only beside the diff, so a remembered
    /// `show` closes it here instead.
    pub(crate) fn start_tree(&mut self, remembered: bool) {
        if !self.tree_shown || self.side_tree() {
            return;
        }
        if remembered {
            self.show_tree(false);
        } else {
            self.pane = Pane::Tree;
        }
    }

    /// Whether the layout splits at this width. Split asked for still
    /// renders a hunk unified when its halves would be too thin to read
    /// ([`Layout::arrangement`]); split remembered from an earlier run renders
    /// unified on a phone-sized screen, without forgetting it.
    fn splits(&self) -> bool {
        match self.view {
            View::Split => !self.view_remembered || self.side_tree(),
            View::Unified => false,
            View::Auto => self.screen.width() >= self.split_min_width,
        }
    }

    fn relayout(&mut self) {
        // The last column is the overview's, on the frame's right border.
        let layout = Layout {
            split: self.splits(),
            width: Some(
                self.screen
                    .width()
                    .saturating_sub(self.content_x() + 1)
                    .max(1),
            ),
            wrap: self.wrap,
            words: false,
            tab_width: self.tab_width,
            indicators: self.indicators,
            one_sided: false,
            digits: 0,
            inline_signs: false,
            scroll: if self.wrap { 0 } else { self.scroll },
            line_numbers: self.line_numbers,
            hunk_headers: self.hunk_headers,
            modified_lines: self.modified_lines == ModifiedLines::Changed,
        };
        // Its rows may change under the selection, which holds them.
        let old = self.doc.layout;
        if (old.split, old.width, old.wrap, old.line_numbers)
            != (layout.split, layout.width, layout.wrap, layout.line_numbers)
        {
            self.doc.selection = None;
        }
        self.doc.layout = layout;
        self.doc.height = self.screen.height().saturating_sub(2);
        self.tree.set_height(self.tree_rows().1);
        self.doc.normalize();
        self.dirty = true;
    }

    pub(crate) fn resize(&mut self, width: usize, height: usize) {
        self.screen.resize(width, height);
        // A panel that became an overlay would cover the diff unasked.
        if !self.side_tree() && self.pane == Pane::Diff {
            self.tree_shown = false;
        }
        self.relayout();
    }

    /// Redraws everything on the next frame; the terminal was used by
    /// something else.
    pub(crate) fn invalidate(&mut self) {
        self.screen.invalidate();
        self.dirty = true;
    }

    pub(crate) fn message(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), Instant::now() + MESSAGE_TIME));
        self.tip = false;
        self.dirty = true;
    }

    /// When the loop should wake up without an event.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let message = self.message.as_ref().map(|(_, until)| *until);
        // The terminal sends nothing while the pointer rests past the edge.
        let edge = self.drag_edge.map(|_| Instant::now() + EDGE_SCROLL);
        let remember = self.remembered_at.map(|at| at + REMEMBER_DELAY);
        let flash = self.commands.as_ref().and_then(Commands::deadline);
        message
            .into_iter()
            .chain(edge)
            .chain(remember)
            .chain(flash.filter(|at| *at > Instant::now()))
            .min()
    }

    pub(crate) fn mouse(&self) -> bool {
        self.mouse
    }

    pub(crate) fn event(&mut self, event: Event) -> Option<Action> {
        match event {
            Event::Input(input) => return self.input(input),
            Event::Enumerated(change) => {
                self.tree.insert(self.doc.files.len(), &change.path);
                self.doc.push(change, Source::Repo);
                self.check_focus();
                self.dirty = true;
            }
            Event::Piped(item) => {
                let file = self.doc.files.len();
                match item {
                    piped::Item::Commit(commit) => {
                        let change = FileChange {
                            path: commit.label().into(),
                            status: Status::Modified,
                            old: None,
                            new: None,
                            staging: None,
                        };
                        self.tree.insert_commit(file, &change.path);
                        self.doc.push(change, Source::Commit(commit));
                    }
                    piped::Item::File(change, piped) => {
                        self.tree.insert(file, &change.path);
                        self.doc.push(change, Source::Piped(piped));
                    }
                }
                if self.doc.files.len() == piped::MAX_ENTRIES {
                    self.message(format!(
                        "stopped reading at {} entries; the rest of the input isn't read",
                        thousands(piped::MAX_ENTRIES)
                    ));
                }
                self.dirty = true;
            }
            Event::EnumerationDone(error) => {
                self.enumerating = false;
                if let Some(error) = error {
                    self.message(format!("listing changes failed: {error}"));
                }
                if !self.piped && self.doc.sort_by_path() {
                    // Positions moved, so a search's marks and counts are
                    // void. Work asked for by position checks the version
                    // of the entry now there.
                    if let Some(search) = &mut self.search {
                        search.counts = vec![None; self.doc.files.len()];
                        search.pending = None;
                        self.doc.search = None;
                    }
                    self.rebuild_tree();
                    if let Some(picker) = &mut self.picker {
                        picker.rebuild(&self.doc.files);
                    }
                }
                self.count_pass = true;
                self.dirty = true;
            }
            Event::Diffed(done) => self.done(done),
            Event::Refreshed(update) => self.refreshed(&update),
            Event::RefreshFailed(error) => self.message(format!("refreshing failed: {error}")),
            Event::Others(others) => {
                self.others = others;
                self.dirty |= self.doc.files.is_empty();
            }
            Event::Watch(event) => self.watch_event(event),
            Event::Tick => {
                // A row's flash ends.
                self.dirty |= self.commands.is_some();
                if let Some((x, y)) = self.drag_edge {
                    self.drag(x, y);
                    self.dirty = true;
                }
                if self
                    .message
                    .as_ref()
                    .is_some_and(|(_, until)| Instant::now() >= *until)
                {
                    self.message = None;
                    self.tip = false;
                    self.dirty = true;
                }
            }
            // The loop handles these; they need the terminal.
            Event::Resize | Event::Interrupt => {}
        }
        None
    }

    /// Takes a refreshed file list. A search's counts and marks stay with
    /// the files they're of, wherever those moved, unless they changed.
    fn refreshed(&mut self, update: &Update) {
        // By position, to find where each file went.
        let before: Vec<(BString, u64)> = if self.search.is_some() {
            self.doc
                .files
                .iter()
                .map(|e| (e.change.path.clone(), e.version))
                .collect()
        } else {
            Vec::new()
        };
        let marks = self.doc.search.take();
        self.doc.replace(update);
        if let Some(search) = &mut self.search {
            let now: HashMap<&BString, (usize, u64)> = self
                .doc
                .files
                .iter()
                .enumerate()
                .map(|(i, e)| (&e.change.path, (i, e.version)))
                .collect();
            // Where the file at `file` went; `None` when it's gone or, with
            // `same`, changed.
            let moved = |file: usize, same: bool| {
                let (path, version) = before.get(file)?;
                let &(to, current) = now.get(path)?;
                (!same || current == *version).then_some(to)
            };
            let mut counts = vec![None; self.doc.files.len()];
            for (file, count) in search.counts.iter().enumerate() {
                if let (Some(count), Some(to)) = (count, moved(file, true)) {
                    counts[to] = Some(*count);
                }
            }
            search.counts = counts;
            search.pending = search
                .pending
                .take()
                .and_then(|(file, land)| Some((moved(file, false)?, land)));
            self.doc.search = marks.and_then(|mut m| {
                m.file = moved(m.file, true)?;
                Some(m)
            });
        }
        self.rebuild_tree();
        if let Some(picker) = &mut self.picker {
            picker.rebuild(&self.doc.files);
        }
        self.count_pass = true;
        self.dirty = true;
    }

    fn watch_event(&mut self, event: client::Event) {
        if self.watch == Watch::Off {
            return;
        }
        match event {
            client::Event::Ready { backend, .. } => {
                self.watch = if backend == "polling" {
                    Watch::Polling
                } else {
                    Watch::Watching
                };
            }
            client::Event::Notice(Notice::Polling(reason)) => {
                self.watch = Watch::Polling;
                self.message(reason);
            }
            client::Event::Local(reason) => self.message(reason),
            client::Event::Notice(_) => {}
        }
        self.dirty = true;
    }

    /// Takes a worker's result if it's still for the file at its position:
    /// work for a file that changed or moved since it was asked for is
    /// dropped. The job for the file as it is now was asked for with the
    /// draw that followed the change, under its own key.
    fn done(&mut self, done: Done) {
        if let Some((file, version)) = done.file()
            && !self.doc.is_current(file, version)
        {
            return;
        }
        match done {
            Done::Search {
                file, id, count, ..
            } => {
                let marked = self.doc.search.as_ref().is_some_and(|m| m.file == file);
                if let Some(search) = &mut self.search
                    && search.id == id
                    && !marked
                {
                    if search.counts.len() <= file {
                        search.counts.resize(file + 1, None);
                    }
                    search.counts[file] = Some(count.unwrap_or(0));
                    self.dirty = true;
                }
            }
            Done::Found {
                file,
                id,
                diff,
                hidden,
                matches,
                ..
            } => self.found(file, id, &diff, &hidden, matches),
            Done::Diff { file, result, .. } => {
                if self.doc.search.as_ref().is_some_and(|m| m.file == file) {
                    self.doc.search = None;
                }
                if self.doc.load(file, result) {
                    self.doc.evict(CACHE_BYTES, &self.visible);
                    self.doc.normalize();
                    self.dirty = true;
                }
            }
            // A result for a diff since replaced is dropped, but still draws
            // again: the job running for it held back the same job for the
            // current diff, and jobs are only asked for after a draw.
            Done::Emphasis {
                file,
                diff,
                hunk,
                emphasis,
                syntax,
                ..
            } => {
                self.doc.set_emphasis(file, &diff, hunk, emphasis, syntax);
                self.dirty |= self.visible.contains(&file);
            }
            Done::GapSyntax {
                file,
                diff,
                gap,
                spans,
                ..
            } => {
                self.doc.set_gap_syntax(file, &diff, gap, spans);
                self.dirty |= self.visible.contains(&file);
            }
            Done::Structural {
                file, diff, result, ..
            } => {
                if self.doc.set_structural(file, &diff, result) {
                    self.doc.evict(CACHE_BYTES, &self.visible);
                    if self.doc.structural {
                        self.doc.normalize();
                        self.dirty = true;
                    }
                } else {
                    self.dirty |= self.doc.structural && self.visible.contains(&file);
                }
            }
            Done::Rendered {
                file,
                id,
                diff,
                result,
                ..
            } if id == self.render_id => {
                self.rendered(file, &diff, result);
            }
            Done::Counts { counts } => {
                for (file, version, counts) in counts {
                    if self.doc.is_current(file, version) {
                        self.doc.set_counts(file, counts);
                    }
                }
                self.dirty |= self.tree_shown;
            }
            Done::Rendered { .. } => {}
        }
    }

    /// A rendering of `diff` arrived, or why there's none; a cancelled one
    /// (`Err(None)`) is asked for again if it's still wanted.
    fn rendered(
        &mut self,
        file: usize,
        diff: &Arc<FileDiff>,
        result: Result<Box<view::Loaded>, Option<String>>,
    ) {
        let result = match result {
            Ok(loaded) => Ok(loaded),
            Err(Some(reason)) => Err(reason),
            Err(None) => return,
        };
        let failed = result.as_ref().err().cloned();
        if self.doc.set_rendered(file, diff, result) {
            self.doc.evict(CACHE_BYTES, &self.visible);
            if let Some(reason) = failed.filter(|_| self.doc.rendered) {
                self.message(format!("{reason}; showing the lines"));
            }
            self.dirty = true;
        }
    }

    /// The count pass to start, once per list and only while the tree
    /// shows: the files on screen in the tree first, then the rest in
    /// order.
    pub(crate) fn take_count_pass(&mut self) -> Option<Vec<Counted>> {
        if !self.count_pass || !self.tree_shown || self.enumerating {
            return None;
        }
        self.count_pass = false;
        let files = &self.doc.files;
        let mut first = self.tree.files_on_screen();
        first.retain(|&f| files.get(f).is_some_and(view::Entry::wants_counts));
        let rest = (0..files.len()).filter(|f| files[*f].wants_counts() && !first.contains(f));
        Some(
            first
                .iter()
                .copied()
                .chain(rest)
                .map(|f| (f, files[f].version, files[f].change.clone()))
                .collect(),
        )
    }

    fn show_tree(&mut self, shown: bool) {
        self.tree_shown = shown;
        if shown {
            self.count_pass = true;
        } else {
            self.pane = Pane::Diff;
        }
        self.relayout();
    }

    /// Rebuilds the tree for a new file list, its selection on the file
    /// shown, which the list keeps when it can.
    fn rebuild_tree(&mut self) {
        self.tree.rebuild(&self.doc.files);
        if !self.doc.files.is_empty() {
            self.tree.follow(self.doc.pos.file);
        }
    }

    /// Gives the tree the keys, its selection starting on the file at the
    /// top of the diff.
    fn focus_tree(&mut self) {
        if !self.tree_shown {
            self.show_tree(true);
        }
        self.pane = Pane::Tree;
        if !self.doc.files.is_empty() {
            self.tree.follow(self.doc.pos.file);
        }
    }

    /// Whether to ask the terminal for its background (OSC 11): only for
    /// tints, and only when the config leaves it to the terminal.
    pub(crate) fn wants_background(&self) -> bool {
        self.ask_background
    }

    /// The background query went out now.
    pub(crate) fn asked_background(&mut self) {
        self.asked = Some(Instant::now());
    }

    fn input(&mut self, input: Input) -> Option<Action> {
        if let Input::Background { rgb } = input {
            // A late reply is ignored: switching tints well after the
            // first paint would read as a glitch.
            if self
                .asked
                .is_some_and(|at| at.elapsed() <= BACKGROUND_REPLY)
            {
                self.background = rgb;
                self.theme.tint(rgb);
                self.dirty = true;
            }
            self.asked = None;
            return None;
        }
        self.dirty = true;
        self.focus = None;
        if std::mem::take(&mut self.tip) {
            self.message = None;
        }
        if self.commands.is_some() {
            return self.commands_input(input);
        }
        if let Some(help) = &mut self.help {
            match input {
                Input::Key(key, mods) if !help.key(&self.keys, key, mods) => self.help = None,
                Input::Scroll { delta, .. } => help.scroll_by(isize::from(delta) * WHEEL_ROWS),
                Input::Click { x, y } => help.tap(usize::from(x), usize::from(y)),
                _ => {}
            }
            return None;
        }
        if self.picker.is_some() {
            return self.picker_input(input);
        }
        let pending = self.doc.pending_land.take();
        let (key, mods) = match input {
            Input::Key(key, mods) => (key, mods),
            Input::Background { .. } => return None,
            Input::Scroll { delta, x, .. } => {
                let rows = isize::from(delta) * WHEEL_ROWS;
                if self.over_tree(usize::from(x)) {
                    self.tree.scroll_by(rows);
                } else {
                    self.doc.scroll(rows);
                }
                return None;
            }
            Input::Click { x, y } => {
                self.click(usize::from(x), usize::from(y));
                return None;
            }
            Input::Drag { x, y } => {
                self.drag(usize::from(x), usize::from(y));
                return None;
            }
            Input::Release { .. } => return self.release(),
        };
        if self.tree.editing && !mods.contains(Mods::CTRL) {
            self.filter_key(key);
            return None;
        }
        if self.search.as_ref().is_some_and(|s| s.editing) {
            self.search_key(key, mods);
            return None;
        }
        let mut action = self.keys.action(self.pane == Pane::Tree, key, mods)?;
        // `esc` backs out one layer at a time: overlays (above), then the
        // search or the tree's filter, the selection, the tree's focus;
        // with nothing left, it says how to quit, or does what it's bound
        // to (`keybind = esc=quit`).
        let layered =
            self.pane == Pane::Tree || self.search.is_some() || self.doc.selection.is_some();
        if key == Key::Esc && layered {
            action = Act::Cancel;
        }
        if self.search.is_some() && self.pane == Pane::Diff {
            match action {
                Act::Cancel => {
                    self.clear_search();
                    return None;
                }
                Act::NextChange | Act::PrevChange if self.doc.selection.is_none() => {
                    self.search_step(action == Act::NextChange, false);
                    return None;
                }
                _ => {}
            }
        }
        // `n` or `N` again before the file they opened is diffed would skip
        // its changes; they wait for it instead.
        if matches!(action, Act::NextChange | Act::PrevChange)
            && pending.is_some_and(|(at, _)| at == self.doc.pos)
        {
            self.doc.pending_land = pending;
            self.message("waiting for this file's diff");
            return None;
        }
        if self.doc.selection.is_some() && self.pane == Pane::Diff {
            match self.selecting(action) {
                Ok(done) => return done,
                Err(()) => self.doc.selection = None,
            }
        }
        self.act(action)
    }

    /// Input while the command panel is open: a row's key or `enter` runs
    /// it, toggles keeping the panel open with their new state flashing;
    /// see [`Commands`] for the rest.
    fn commands_input(&mut self, input: Input) -> Option<Action> {
        let entries = self.command_entries();
        let commands = self.commands.as_mut()?;
        let outcome = match input {
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
    fn command_entries(&self) -> Vec<commands::Entry> {
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
                    // The tree is somewhere to go, not a look to set.
                    stays: setting && action != Act::ToggleTree,
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
            Act::ToggleSyntax => toggle(self.doc.structural),
            Act::ToggleRendered => toggle(self.doc.rendered),
            Act::ToggleHighlight => toggle(self.highlight),
            Act::ToggleWhitespace => toggle(self.whitespace != Whitespace::Exact),
            Act::ToggleLineNumbers => toggle(self.line_numbers),
            Act::ToggleHunkHeaders => toggle(self.hunk_headers),
            Act::ToggleCopyDecorations => toggle(self.copy_decorations),
            Act::ToggleMouse => toggle(self.mouse),
            Act::ToggleWatch => toggle(self.watch != Watch::Off),
            Act::ToggleTree => toggle(self.tree_shown),
            Act::ToggleFull => toggle(
                self.doc
                    .files
                    .get(self.doc.pos.file)
                    .is_some_and(|e| self.doc.is_full(e)),
            ),
            _ => (None, title.to_owned(), None),
        }
    }

    /// The note after a command from the menu: the state it set, said
    /// plainly (`syntax diff on`, `layout: auto (split at this width)`).
    fn state_note(&self, action: Act) -> Option<String> {
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
            Act::ToggleSyntax => format!("{label} {}", on(self.doc.structural)),
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

    /// Input while the picker is open. Letters go to the query, `j` and `k`
    /// included; arrows, `ctrl+n`/`ctrl+p`, the wheel and taps move or pick.
    fn picker_input(&mut self, input: Input) -> Option<Action> {
        let top = self.picker_top();
        let files = &self.doc.files;
        let picker = self.picker.as_mut()?;
        // The rows on screen are what the keys act on; files that arrived
        // since go in first so a narrowed query sees them.
        picker.catch_up(files);
        let ctrl = |mods: Mods| mods.contains(Mods::CTRL);
        let pick = match input {
            Input::Key(Key::Esc, _) => {
                self.picker = None;
                return None;
            }
            Input::Key(Key::Char('c'), mods) if ctrl(mods) => return Some(Action::Quit),
            Input::Key(Key::Enter, _) => picker.file(),
            Input::Key(Key::Up, _) => {
                picker.step(-1);
                None
            }
            Input::Key(Key::Char('p'), mods) if ctrl(mods) => {
                picker.step(-1);
                None
            }
            Input::Key(Key::Down, _) => {
                picker.step(1);
                None
            }
            Input::Key(Key::Char('n'), mods) if ctrl(mods) => {
                picker.step(1);
                None
            }
            Input::Key(Key::PageUp, _) => {
                picker.step(-picker.page());
                None
            }
            Input::Key(Key::PageDown, _) => {
                picker.step(picker.page());
                None
            }
            Input::Key(Key::Backspace, _) => {
                picker.pop(files);
                None
            }
            Input::Key(Key::Char(c), mods)
                if !ctrl(mods) && (c.is_alphanumeric() || "._-/".contains(c)) =>
            {
                picker.push(files, c);
                None
            }
            Input::Scroll { delta, .. } => {
                picker.step(isize::from(delta));
                None
            }
            // The query is the picker's first row.
            Input::Click { y, .. } => usize::from(y)
                .checked_sub(top + 1)
                .and_then(|row| picker.click(row)),
            _ => None,
        };
        if let Some(file) = pick {
            self.picker = None;
            self.open(file);
        }
        None
    }

    /// An action while lines are selected: `Ok` with its result when the
    /// selection takes it, `Err` when it ends the selection and runs as
    /// usual.
    fn selecting(&mut self, action: Act) -> Result<Option<Action>, ()> {
        let height = isize::try_from(self.doc.height.max(1)).unwrap_or(isize::MAX);
        match action {
            Act::ScrollDown => self.doc.select_by(1),
            Act::ScrollUp => self.doc.select_by(-1),
            Act::PageDown => self.doc.select_by(height),
            Act::PageUp => self.doc.select_by(-height),
            Act::HalfPageDown => self.doc.select_by((height / 2).max(1)),
            Act::HalfPageUp => self.doc.select_by(-(height / 2).max(1)),
            Act::Top => self.doc.select_by(isize::MIN + 1),
            Act::Bottom => self.doc.select_by(isize::MAX),
            Act::Copy => {
                let done = self.copy();
                // Refused as too large, it stays for trimming.
                if done.is_some() {
                    self.doc.selection = None;
                }
                return Ok(done);
            }
            Act::CycleLayout => {
                if !self.doc.select_other_side() {
                    self.message("these rows have no lines on the other side");
                }
            }
            Act::Select | Act::Cancel => self.doc.selection = None,
            // The panel's copy acts on the selection, so opening it keeps it.
            Act::ScrollLeft
            | Act::ScrollRight
            | Act::ScrollReset
            | Act::CopyPath
            | Act::Help
            | Act::Leader
            | Act::Quit => return Ok(self.act(action)),
            _ => return Err(()),
        }
        Ok(None)
    }

    /// Runs `action` for the pane with the keys.
    fn act(&mut self, action: Act) -> Option<Action> {
        let height = isize::try_from(self.doc.height.max(1)).unwrap_or(isize::MAX);
        let half = (height / 2).max(1);
        let tree = self.pane == Pane::Tree;
        match action {
            Act::ScrollDown
            | Act::ScrollUp
            | Act::PageDown
            | Act::PageUp
            | Act::Top
            | Act::Bottom
            | Act::Open
            | Act::Collapse
            | Act::MarkViewed
            | Act::Cancel
            | Act::TreeWider
            | Act::TreeNarrower => {
                if tree {
                    self.tree_action(action);
                } else {
                    self.diff_action(action);
                }
            }
            Act::Quit => return Some(Action::Quit),
            Act::SwitchPane => {
                if tree {
                    self.pane = Pane::Diff;
                    if !self.side_tree() {
                        self.show_tree(false);
                    }
                } else {
                    self.focus_tree();
                }
            }
            Act::ToggleTree => self.toggle_tree(),
            Act::OpenPicker => {
                self.doc.selection = None;
                self.picker = Some(Picker::new(&self.doc.files));
            }
            Act::FilterTree => {
                self.focus_tree();
                self.tree.editing = true;
            }
            Act::Search => self.start_search(),
            Act::ParentFolder => {
                // From the diff, the tree starts on the file shown; in the
                // tree, each `u` climbs a folder further.
                if !tree {
                    self.focus_tree();
                }
                self.tree.select_parent();
            }
            Act::NextChange | Act::PrevChange => {
                let before = self.doc.pos.file;
                self.change(action == Act::NextChange);
                // On past files the tree's filter hides.
                if self.doc.pos.file != before && !self.tree.in_scope(self.doc.pos.file) {
                    self.step_file(action == Act::NextChange);
                }
            }
            Act::NextFile | Act::PrevFile => self.step_file(action == Act::NextFile),
            Act::Expand | Act::ToggleFull => self.unhide(action == Act::ToggleFull),
            Act::CycleLayout
            | Act::ToggleWrap
            | Act::ToggleSyntax
            | Act::ToggleRendered
            | Act::CycleIndicators
            | Act::CycleModifiedLines
            | Act::CycleTheme
            | Act::ToggleHighlight
            | Act::ToggleWhitespace
            | Act::ToggleLineNumbers
            | Act::ToggleHunkHeaders
            | Act::ToggleStatusBar
            | Act::ToggleCopyDecorations => return self.set_view(action),
            Act::Leader => self.commands = Some(Commands::default()),
            Act::ToggleWatch => {
                let on = self.watch == Watch::Off;
                self.watch = if on { Watch::Starting } else { Watch::Off };
                self.remember("watch", on.to_string());
                return Some(Action::Watch(on));
            }
            Act::ToggleMouse => return Some(self.toggle_mouse()),
            Act::Edit => return self.edit(),
            Act::Help => self.help = Some(Help::default()),
            Act::ScrollRight | Act::ScrollLeft | Act::ScrollReset => self.scroll_code(action),
            Act::Select => self.start_selection(),
            Act::Copy if self.doc.selection.is_some() => return self.copy(),
            Act::Copy => self.message("nothing selected; v selects lines"),
            Act::CopyPath => return self.copy_path(),
            Act::HalfPageDown => self.doc.scroll(half),
            Act::HalfPageUp => self.doc.scroll(-half),
        }
        None
    }

    /// `M`: takes the mouse, or leaves it to the terminal's own selection.
    fn toggle_mouse(&mut self) -> Action {
        self.mouse = !self.mouse;
        self.remember("mouse", self.mouse.to_string());
        self.press = None;
        self.drag_edge = None;
        self.message(if self.mouse {
            "mouse on: drag over code to select and copy it"
        } else {
            "mouse off: the terminal selects text itself"
        });
        Action::Mouse(self.mouse)
    }

    /// A view setting changed by its key or the command panel, and
    /// remembered for the next run.
    fn set_view(&mut self, action: Act) -> Option<Action> {
        match action {
            Act::CycleLayout => {
                self.view = match self.view {
                    View::Split => View::Unified,
                    View::Unified => View::Auto,
                    View::Auto => View::Split,
                };
                self.view_remembered = false;
                self.remember("view", config::choice_name(&self.view));
                self.relayout();
                if let Some(note) = self.state_note(action) {
                    self.message(note);
                }
            }
            Act::ToggleWrap => {
                self.wrap = !self.wrap;
                self.remember("wrap", self.wrap.to_string());
                self.relayout();
            }
            Act::ToggleSyntax => {
                self.doc.selection = None;
                self.doc.structural = !self.doc.structural;
                self.remember("syntax-diff", self.doc.structural.to_string());
                self.doc.normalize();
                if self.doc.structural && self.doc.is_partial() {
                    self.message(
                        "the syntax diff needs both whole files; a piped diff has only its hunks",
                    );
                }
            }
            Act::ToggleRendered => self.toggle_rendered(),
            Act::CycleIndicators => {
                self.indicators = match self.indicators {
                    Indicators::Auto | Indicators::Background => Indicators::Bars,
                    Indicators::Bars => Indicators::Classic,
                    Indicators::Classic => Indicators::None,
                    Indicators::None => Indicators::Background,
                };
                self.remember("indicators", config::choice_name(&self.indicators));
                self.reload_theme();
                self.relayout();
            }
            Act::CycleModifiedLines => {
                self.modified_lines = match self.modified_lines {
                    ModifiedLines::Changed => ModifiedLines::AddedRemoved,
                    ModifiedLines::AddedRemoved => ModifiedLines::Changed,
                };
                self.remember("modified-lines", config::choice_name(&self.modified_lines));
                self.relayout();
            }
            Act::CycleTheme => self.step_theme(false),
            Act::ToggleHighlight | Act::ToggleWhitespace => return self.rediff(action),
            Act::ToggleLineNumbers => {
                self.line_numbers = !self.line_numbers;
                self.remember("line-numbers", self.line_numbers.to_string());
                self.relayout();
            }
            Act::ToggleHunkHeaders => {
                self.hunk_headers = !self.hunk_headers;
                self.remember("hunk-headers", self.hunk_headers.to_string());
                self.relayout();
            }
            Act::ToggleStatusBar => {
                self.status_bar = match self.status_bar {
                    StatusBar::Bottom => StatusBar::Top,
                    StatusBar::Top => StatusBar::Bottom,
                };
                self.remember("status-bar", config::choice_name(&self.status_bar));
            }
            Act::ToggleCopyDecorations => {
                self.copy_decorations = !self.copy_decorations;
                self.remember("copy-decorations", self.copy_decorations.to_string());
            }
            _ => {}
        }
        None
    }

    /// `r`: rendered views on or off, from a file whose type has a
    /// renderer, which turning them on also needs installed; elsewhere a
    /// hint says why not.
    fn toggle_rendered(&mut self) {
        let on = !self.doc.rendered;
        if let Some(hint) = self.render_hint(on) {
            self.message(hint);
            return;
        }
        self.doc.selection = None;
        self.doc.set_rendered_mode(on);
        self.remember("rendered", on.to_string());
        self.message(format!("rendered view {}", if on { "on" } else { "off" }));
    }

    /// Why `r` does nothing on the file shown, if it doesn't: it has no
    /// renderer, or, with `installed`, its renderer isn't on PATH.
    fn render_hint(&mut self, installed: bool) -> Option<String> {
        let entry = self.doc.files.get(self.doc.pos.file)?;
        if matches!(entry.source, Source::Piped(_)) {
            return Some("a piped diff has only its hunks; nothing to render".to_owned());
        }
        let path = entry.change.path.clone();
        let Some(name) = self.renderers.for_path(&path).map(str::to_owned) else {
            let what = plugin::extension(&path).map_or_else(
                || escape(path.rsplit(|&b| b == b'/').next().unwrap_or(&path)),
                |ext| format!(".{}", escape(ext)),
            );
            return Some(format!("no renderer for {what} (see docs/plugins.md)"));
        };
        (installed && self.program(&name).is_none()).then(|| {
            format!(
                "{}{name} isn't on PATH (see docs/plugins.md)",
                plugin::PREFIX
            )
        })
    }

    /// The renderer `name`, looked up on PATH the first time it's asked
    /// for this session.
    fn program(&mut self, name: &str) -> Option<Program> {
        if let Some((_, found)) = self.programs.iter().find(|(n, _)| n == name) {
            return found.clone();
        }
        let found = plugin::find(name);
        self.programs.push((name.to_owned(), found.clone()));
        found
    }

    /// Columns of a pane's text, about: what renderers are asked to fit.
    fn pane_text_width(&self) -> usize {
        let layout = &self.doc.layout;
        let width = layout.width.unwrap_or(80);
        let pane = if layout.split {
            width.saturating_sub(1) / 2
        } else {
            width
        };
        let numbers = match (layout.line_numbers, layout.split) {
            (false, _) => 0,
            (true, true) => 5,
            (true, false) => 10,
        };
        pane.saturating_sub(layout.lead_width() + numbers + layout.mark_width())
            .max(20)
    }

    /// Settles what the workers should render: the file shown, when
    /// rendered views are on, its type has a renderer that's installed
    /// and it has no rendering yet. A change of target cancels the one
    /// running. A new pane width drops renderings made for the old one.
    fn update_render(&mut self) {
        if self.renderers.is_empty() {
            return;
        }
        let width = self.pane_text_width();
        if width != self.render_width {
            self.render_width = width;
            self.doc.forget_renders(width);
        }
        let target = self.render_target(width);
        let same = match (&target, &self.render) {
            (Some(a), Some(b)) => a.same(b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.render_id += 1;
            let live = if target.is_some() {
                self.render_id
            } else {
                u64::MAX
            };
            self.live_render.store(live, Ordering::Relaxed);
            self.render = target;
        }
    }

    fn render_target(&mut self, width: usize) -> Option<RenderTarget> {
        if !self.doc.rendered {
            return None;
        }
        let file = self.doc.pos.file;
        let entry = self.doc.files.get(file)?;
        let name = self.renderers.for_path(&entry.change.path)?.to_owned();
        let diff = Arc::clone(entry.wants_render()?);
        let program = self.program(&name)?;
        Some(RenderTarget {
            file,
            diff,
            program,
            width,
            light: theme::color::is_light(self.background),
        })
    }

    /// What the status bar says about the file's rendering, when rendered
    /// views are on and its type has a renderer.
    fn render_note(&self) -> Option<String> {
        if !self.doc.rendered || self.renderers.is_empty() {
            return None;
        }
        let entry = self.doc.files.get(self.doc.pos.file)?;
        let name = self.renderers.for_path(&entry.change.path)?;
        match entry.render.as_deref() {
            Some(view::Render::Failed(reason)) => Some(format!("lines: {reason}")),
            None if self
                .render
                .as_ref()
                .is_some_and(|t| t.file == self.doc.pos.file) =>
            {
                Some("rendering…".to_owned())
            }
            None if self
                .programs
                .iter()
                .any(|(n, found)| n == name && found.is_none()) =>
            {
                Some(format!("lines: {}{name} isn't on PATH", plugin::PREFIX))
            }
            _ => None,
        }
    }

    /// The next file (previous, without `forward`) the tree's filter keeps;
    /// every file without one. Stays put when there's none.
    fn step_file(&mut self, forward: bool) {
        let start = self.doc.pos;
        loop {
            let before = self.doc.pos.file;
            if forward {
                self.doc.next_file();
            } else {
                self.doc.prev_file();
            }
            let now = self.doc.pos.file;
            if now == before {
                if !self.tree.in_scope(now) {
                    self.doc.goto_file(start.file);
                    self.message("no more files the tree's filter keeps");
                }
                return;
            }
            if self.tree.in_scope(now) {
                return;
            }
        }
    }

    /// Typing into the tree's filter: `enter` keeps it as the scope of
    /// `f`/`n`, `esc` clears it.
    fn filter_key(&mut self, key: Key) {
        let mut filter = self.tree.filter().to_owned();
        match key {
            Key::Enter => {
                self.tree.editing = false;
                return;
            }
            Key::Esc => {
                self.tree.editing = false;
                filter.clear();
            }
            Key::Backspace => {
                filter.pop();
            }
            Key::Char(c) => filter.push(c),
            _ => return,
        }
        self.tree.set_filter(&filter);
        if let Some(Item::File(file)) = self.tree.step(0) {
            self.doc.goto_file(file);
        }
    }

    /// `/` in the diff: a search line at the bottom, matched as it's typed.
    fn start_search(&mut self) {
        self.doc.selection = None;
        self.search_id += 1;
        let id = self.search_id;
        self.search = Some(Search {
            needle: Arc::new(Needle::new("")),
            editing: true,
            id,
            counts: vec![None; self.doc.files.len()],
            pending: None,
        });
        self.live_search.store(id, Ordering::Relaxed);
        self.doc.search = None;
    }

    /// Typing into the search: the workers look for the new pattern and
    /// matches show as they arrive; `enter` goes to the first at or after
    /// the view, `esc` gives up.
    fn search_key(&mut self, key: Key, mods: Mods) {
        let Some(search) = &mut self.search else {
            return;
        };
        let mut text = search.needle.text.clone();
        match key {
            Key::Esc => return self.clear_search(),
            Key::Enter => {
                search.editing = false;
                if text.is_empty() {
                    return self.clear_search();
                }
                self.search_step(true, true);
                return;
            }
            Key::Backspace => {
                text.pop();
            }
            Key::Char(c) if !mods.contains(Mods::CTRL) => text.push(c),
            _ => return,
        }
        search.needle = Arc::new(Needle::new(&text));
        self.search_id += 1;
        search.id = self.search_id;
        search.counts = vec![None; self.doc.files.len()];
        search.pending = None;
        self.live_search.store(search.id, Ordering::Relaxed);
        self.doc.search = None;
    }

    fn clear_search(&mut self) {
        self.search = None;
        self.live_search.store(u64::MAX, Ordering::Relaxed);
        self.doc.search = None;
        self.message("search cleared; n/N go between changes again");
    }

    /// A file's matches from a worker. They show if they're of the search,
    /// the diff and the revealed lines as they are now, and an `n` or `N`
    /// that came before them lands in them.
    fn found(
        &mut self,
        file: usize,
        id: u64,
        diff: &Arc<FileDiff>,
        hidden: &[Range<usize>],
        matches: Option<Matches>,
    ) {
        if self.search.as_ref().is_none_or(|s| s.id != id) {
            return;
        }
        // A scan stopped for a newer search.
        let Some(matches) = matches else { return };
        self.dirty = true;
        let current = self.doc.diff_of(file).is_some_and(|d| Arc::ptr_eq(d, diff))
            && self.doc.hidden_lines(file).as_deref() == Some(hidden);
        let Some(search) = self.search.as_mut().filter(|_| current) else {
            return;
        };
        if let Some(count) = search.counts.get_mut(file) {
            *count = Some(matches.list.len());
        }
        let land = search.pending.take_if(|(f, _)| *f == file);
        self.doc.search = Some(Marks {
            file,
            hidden: hidden.to_vec(),
            matches: Arc::new(matches),
            current: None,
        });
        match land.map(|(_, land)| land) {
            Some(Land::First) => self.land_search(0),
            Some(Land::Last) => self.land_search(usize::MAX),
            Some(Land::Step { forward, here }) => self.search_step(forward, here),
            None => {}
        }
    }

    /// Goes to match `i` of the file shown (its last when past the end).
    fn land_search(&mut self, i: usize) {
        let file = self.doc.pos.file;
        let Some(marks) = self
            .doc
            .search
            .as_mut()
            .filter(|m| m.file == file && !m.matches.list.is_empty())
        else {
            return;
        };
        let i = i.min(marks.matches.list.len() - 1);
        marks.current = Some(i);
        let m = marks.matches.list[i];
        if let Some(pos) = self.doc.match_pos(file, &m) {
            self.doc.goto_match(pos);
        }
    }

    /// `n`/`N` while searching: the next or previous match, on into the
    /// next file that has one. With `here`, a match on the row at the
    /// top counts as the next one (`enter` after typing). Before the file's
    /// matches have arrived, it waits for them.
    fn search_step(&mut self, forward: bool, here: bool) {
        let file = self.doc.pos.file;
        let top = Pos {
            offset: 0,
            ..self.doc.pos
        };
        let marks = self
            .doc
            .search
            .as_ref()
            .filter(|m| m.file == file)
            .map(|m| (Arc::clone(&m.matches), m.current));
        // Its diff is coming, or here to scan; not a commit, a failed diff
        // or a combined diff shown as it came, which have no matches.
        let scanned = self.doc.diff_of(file).is_some()
            || self
                .doc
                .files
                .get(file)
                .is_some_and(view::Entry::wants_diff);
        match marks {
            Some((matches, current)) => {
                let n = matches.list.len();
                // The matches are in the order the diff shows them.
                let mut before = |m: &view::Match, or_at: bool| {
                    self.doc
                        .match_pos(file, m)
                        .is_none_or(|p| p < top || (or_at && p == top))
                };
                let next = match (current, forward) {
                    (Some(i), true) => (i + 1 < n).then_some(i + 1),
                    (Some(i), false) => i.checked_sub(1),
                    (None, true) => {
                        let at = matches.list.partition_point(|m| before(m, !here));
                        (at < n).then_some(at)
                    }
                    (None, false) => matches
                        .list
                        .partition_point(|m| before(m, false))
                        .checked_sub(1),
                };
                if let Some(i) = next {
                    self.land_search(i);
                    return;
                }
            }
            None if scanned => {
                if let Some(search) = &mut self.search {
                    search.pending = Some((file, Land::Step { forward, here }));
                }
                self.message("searching this file…");
                return;
            }
            None => {}
        }
        let counts = self.search_counts();
        let Some(search) = &mut self.search else {
            return;
        };
        let others: Vec<usize> = if forward {
            (file + 1..counts.len()).collect()
        } else {
            (0..file).rev().collect()
        };
        let target = others
            .iter()
            .copied()
            .find(|&f| counts[f].is_some_and(|n| n > 0));
        let unknown = others.iter().any(|&f| counts[f].is_none());
        match target {
            Some(f) => {
                search.pending = Some((f, if forward { Land::First } else { Land::Last }));
                self.doc.goto_file(f);
                let arrow = if forward {
                    "→ next file"
                } else {
                    "← previous file"
                };
                let status = self.search_status().unwrap_or_default();
                self.message(format!("{arrow} · {status}"));
            }
            None if unknown => self.message("still searching the other files…"),
            None => self.message(if forward {
                "no more matches below"
            } else {
                "no more matches above"
            }),
        }
    }

    /// Each entry's matches, once known: a commit's header has none to
    /// search, nor has the file shown when there's no diff of it to scan
    /// (it failed, or it's shown as it came); entries that arrived since
    /// the search began are still to be counted.
    fn search_counts(&self) -> Vec<Option<usize>> {
        let Some(search) = &self.search else {
            return Vec::new();
        };
        let shown = self.doc.pos.file;
        let unscanned = |f: usize, e: &view::Entry| {
            f == shown && !e.wants_diff() && self.doc.diff_of(f).is_none()
        };
        self.doc
            .files
            .iter()
            .enumerate()
            .map(|(f, e)| {
                if e.is_commit() || unscanned(f, e) {
                    Some(0)
                } else {
                    search.counts.get(f).copied().flatten()
                }
            })
            .collect()
    }

    /// `match 3/17` for the status bar, `17+` while files are still
    /// being counted.
    fn search_status(&self) -> Option<String> {
        self.search.as_ref().filter(|s| !s.editing)?;
        let counts = self.search_counts();
        let total: usize = counts.iter().flatten().sum();
        let more = if counts.iter().any(Option::is_none) {
            "+"
        } else {
            ""
        };
        let at = self
            .doc
            .search
            .as_ref()
            .and_then(|m| m.current.map(|i| (m.file, i)));
        let index = at.map(|(file, i)| {
            counts[..file.min(counts.len())]
                .iter()
                .flatten()
                .sum::<usize>()
                + i
                + 1
        });
        Some(match index {
            Some(i) => format!("match {i}/{total}{more}"),
            None => format!("{total}{more} matches"),
        })
    }

    /// `t`: the tree shown or hidden, remembered for the next run. On a
    /// narrow screen it opens over the diff, with the keys.
    fn toggle_tree(&mut self) {
        if self.tree_shown {
            self.show_tree(false);
        } else if self.side_tree() {
            self.show_tree(true);
        } else {
            self.focus_tree();
        }
        // An overlay is a moment's look, not a choice for the next run.
        if self.side_tree() {
            self.remember("tree", if self.tree_shown { "show" } else { "hide" });
        }
    }

    /// Every theme, built-in and the user's, read again.
    fn load_themes(&mut self) {
        self.themes = theme::available()
            .into_iter()
            .map(|(name, _)| {
                let theme =
                    Theme::load(&name, &mut Vec::new()).unwrap_or_else(|_| Theme::default_theme());
                (name, theme)
            })
            .collect();
    }

    /// The next theme, or with `back` the one before, made the choice.
    fn step_theme(&mut self, back: bool) {
        if !self.color {
            self.message("colors are off; themes don't show");
            return;
        }
        if self.themes.is_empty() {
            self.load_themes();
        }
        let names: Vec<&str> = self.themes.iter().map(|(n, _)| n.as_str()).collect();
        if let Some(name) = theme::step_name(&names, &self.theme_name, back).map(str::to_owned) {
            self.theme_name.clone_from(&name);
            self.remember("theme", name);
            self.reload_theme();
        }
    }

    /// The theme again, tinted for the indicators: after another theme was
    /// picked or the indicators changed.
    fn reload_theme(&mut self) {
        if !self.color {
            return;
        }
        if self.themes.is_empty() {
            self.load_themes();
        }
        let mut theme = match self.themes.iter().find(|(n, _)| *n == self.theme_name) {
            Some((_, theme)) => theme.clone(),
            None => Theme::load(&self.theme_name, &mut Vec::new())
                .unwrap_or_else(|_| Theme::default_theme()),
        };
        if self.indicators == Indicators::Background {
            theme.tint(self.background);
        }
        self.theme = theme;
        self.screen.invalidate();
    }

    /// Highlighting or whitespace toggled: every file is diffed again with
    /// the new settings.
    fn rediff(&mut self, action: Act) -> Option<Action> {
        if self.piped {
            self.message("a piped diff is shown as it came, without highlighting");
            return None;
        }
        if action == Act::ToggleHighlight {
            self.highlight = !self.highlight;
            self.remember("highlight", self.highlight.to_string());
        } else {
            self.whitespace = if self.whitespace == Whitespace::Exact {
                Whitespace::IgnoreAll
            } else {
                Whitespace::Exact
            };
            let all = self.whitespace == Whitespace::IgnoreAll;
            self.remember("ignore-all-space", all.to_string());
            self.remember("ignore-space-change", "false");
        }
        self.doc.restale();
        self.count_pass = true;
        Some(Action::Rediff {
            highlight: self.highlight,
            whitespace: self.whitespace,
        })
    }

    /// `x`, or `X` (`full`): more of the unchanged lines, which a piped
    /// diff doesn't have. `x` reveals some in this file for this run; `X`
    /// switches every file and is remembered.
    fn unhide(&mut self, full: bool) {
        if self.doc.is_partial() || (full && self.piped) {
            self.message("a piped diff has only its hunks; the lines between them aren't in it");
        } else if full {
            let before = self.doc.full;
            self.doc.toggle_full();
            if self.doc.full != before {
                let context = if self.doc.full {
                    "full".to_owned()
                } else {
                    self.context_lines.to_string()
                };
                self.remember("context", context);
            }
        } else if !self.doc.expand_target() {
            self.message("no hidden lines here");
        }
    }

    /// A move while the diff has the keys; tree actions do nothing here.
    fn diff_action(&mut self, action: Act) {
        let height = isize::try_from(self.doc.height.max(1)).unwrap_or(isize::MAX);
        match action {
            Act::ScrollDown => self.doc.scroll(1),
            Act::ScrollUp => self.doc.scroll(-1),
            Act::PageDown => self.doc.scroll(height),
            Act::PageUp => self.doc.scroll(-height),
            Act::Top => self.doc.top(),
            Act::Bottom => self.doc.bottom(),
            // Nothing left to back out of.
            Act::Cancel => {
                let quit = self
                    .key_name(Act::Quit)
                    .unwrap_or_else(|| "ctrl+c".to_owned());
                self.message(format!("press {quit} to quit"));
            }
            _ => {}
        }
    }

    /// `n` (or `N`, not `next`): the next change, saying so when it's in
    /// another file.
    fn change(&mut self, next: bool) {
        let from = self.doc.pos.file;
        if next {
            if let Some(file) = self.doc.next_change() {
                self.note_file("→", "next file", from, file);
            }
        } else if let Some(file) = self.doc.prev_change() {
            self.note_file("←", "previous file", from, file);
        }
    }

    /// Says which file `n` or `N` moved to, or which commit when it moved
    /// into another.
    fn note_file(&mut self, arrow: &str, what: &str, from: usize, file: usize) {
        let commit = self.doc.commit_of(file);
        if commit != self.doc.commit_of(from)
            && let Some(c) = commit
        {
            let label = escape(&self.doc.files[self.doc.commits[c]].change.path);
            let total = self.doc.commits.len();
            self.message(format!("{arrow} commit {}/{total}: {label}", c + 1));
            return;
        }
        let path = escape(&self.doc.files[file].change.path);
        self.message(format!("{arrow} {what}: {path}"));
    }

    /// `<`, `>` and `0`: the code scrolls sideways, as far as the widest
    /// line on screen needs.
    fn scroll_code(&mut self, action: Act) {
        if self.wrap {
            self.message("long lines wrap; W turns wrapping off to scroll sideways");
            return;
        }
        self.scroll = match action {
            Act::ScrollRight if self.scroll + SCROLL_COLUMNS < self.doc.widest() => {
                self.scroll + SCROLL_COLUMNS
            }
            Act::ScrollLeft => self.scroll.saturating_sub(SCROLL_COLUMNS),
            Act::ScrollReset => 0,
            _ => self.scroll,
        };
        self.doc.layout.scroll = self.scroll;
    }

    /// `v`: selects from the row last tapped when it's on screen, or from
    /// the first line of code on screen.
    fn start_selection(&mut self) {
        if self.pane == Pane::Tree {
            self.pane = Pane::Diff;
            if !self.side_tree() {
                self.show_tree(false);
            }
        }
        let tapped = self
            .tapped
            .take()
            .filter(|(pos, _)| (0..self.doc.height).any(|y| self.doc.row_at(y) == Some(*pos)));
        if !self.doc.select(tapped) {
            self.message("no lines of code here to select");
        }
    }

    /// `y`: the selected lines on the clipboard.
    fn copy(&mut self) -> Option<Action> {
        let copied = self.doc.selected(self.copy_decorations)?;
        if copied.text.len() > term::MAX_CLIPBOARD_BYTES {
            self.message(format!(
                "{} lines are too many to copy: {}KB, over the {}KB a copy can hold",
                copied.lines,
                copied.text.len() / 1000,
                term::MAX_CLIPBOARD_BYTES / 1000
            ));
            return None;
        }
        let (n, unit) = match copied.chars {
            Some(chars) => (chars, "char"),
            None => (copied.lines, "line"),
        };
        let s = if n == 1 { "" } else { "s" };
        let mut note = format!("copied {n} {unit}{s}");
        if copied.decorated {
            note.push_str(" with decorations");
        }
        if copied.hidden > 0 {
            let _ = write!(note, ", {} of them hidden", copied.hidden);
        }
        if copied.missing > 0 {
            let _ = write!(
                note,
                "; {} lines between hunks aren't in the piped diff",
                copied.missing
            );
        }
        Some(Action::Copy {
            text: copied.text,
            note,
        })
    }

    /// `Y`: the path of the file shown, as the repository has it.
    fn copy_path(&mut self) -> Option<Action> {
        let entry = self.doc.files.get(self.doc.pos.file)?;
        let path = match &entry.source {
            Source::Commit(commit) => commit.id.as_ref()?.as_bytes().into(),
            _ => entry.change.path.clone(),
        };
        Some(Action::Copy {
            note: format!("copied {}", escape(&path)),
            text: path.into(),
        })
    }

    /// An action while the tree has the keys.
    fn tree_action(&mut self, action: Act) {
        let moved = match action {
            Act::ScrollDown => self.tree.step(1),
            Act::ScrollUp => self.tree.step(-1),
            Act::PageDown => self.tree.page(true),
            Act::PageUp => self.tree.page(false),
            Act::Top => self.tree.step(isize::MIN),
            Act::Bottom => self.tree.step(isize::MAX),
            Act::Open => {
                match self.tree.item() {
                    Some(Item::File(file)) => self.open(file),
                    Some(Item::Dir { open } | Item::Commit { open, .. }) => {
                        self.tree.set_open(!open);
                    }
                    None => {}
                }
                None
            }
            Act::Collapse => {
                match self.tree.item() {
                    Some(Item::Dir { open: true } | Item::Commit { open: true, .. }) => {
                        self.tree.set_open(false);
                    }
                    _ => self.tree.select_parent(),
                }
                None
            }
            Act::MarkViewed => {
                if let Some(Item::File(file)) = self.tree.item() {
                    let entry = &mut self.doc.files[file];
                    entry.viewed = !entry.viewed;
                }
                None
            }
            Act::Cancel if !self.tree.filter().is_empty() => {
                self.tree.set_filter("");
                self.message("tree filter cleared");
                None
            }
            Act::Cancel if !self.side_tree() => {
                self.show_tree(false);
                None
            }
            Act::Cancel => {
                self.pane = Pane::Diff;
                None
            }
            Act::TreeWider | Act::TreeNarrower => {
                self.resize_tree(action == Act::TreeWider);
                None
            }
            _ => None,
        };
        if let Some(Item::File(file) | Item::Commit { file, .. }) = moved {
            self.doc.goto_file(file);
        }
    }

    /// Shows `file` in the diff and gives the diff the keys; an overlay
    /// tree closes.
    fn open(&mut self, file: usize) {
        self.doc.goto_file(file);
        self.pane = Pane::Diff;
        if !self.side_tree() {
            self.show_tree(false);
        }
    }

    /// The tree's columns, beside the diff or over it.
    fn tree_columns(&self) -> Option<usize> {
        self.tree_shown
            .then(|| self.side_width().unwrap_or_else(|| self.screen.width()))
    }

    fn over_tree(&self, x: usize) -> bool {
        self.tree_columns().is_some_and(|w| x < w)
    }

    /// The code row at screen cell `x`, `y` and the side it's on there.
    fn code_at(&mut self, x: usize, y: usize) -> Option<(Pos, Side)> {
        let pos = self.doc.row_at(y.checked_sub(1)?)?;
        let side = self.doc.side_at(pos, x.checked_sub(self.content_x())?)?;
        Some((pos, side))
    }

    fn click(&mut self, x: usize, y: usize) {
        self.press = None;
        self.drag_edge = None;
        let count = match self.clicks {
            Some((at, cell, n)) if cell == (x, y) && at.elapsed() <= MULTI_CLICK => n % 3 + 1,
            _ => 1,
        };
        self.clicks = Some((Instant::now(), (x, y), count));
        if self.tree_button == Some((x, y)) {
            self.toggle_tree();
            return;
        }
        let (tree_top, tree_rows) = self.tree_rows();
        let on_tree = self.over_tree(x) && (tree_top..tree_top + tree_rows).contains(&y);
        if !on_tree && (y == 0 || y + 1 >= self.screen.height()) {
            match &self.switcher {
                Some((row, [prev, ..])) if *row == y && prev.contains(&x) => self.doc.prev_file(),
                Some((row, [_, name, _])) if *row == y && name.contains(&x) => {
                    self.picker = Some(Picker::new(&self.doc.files));
                }
                Some((row, [.., next])) if *row == y && next.contains(&x) => self.doc.next_file(),
                _ => {}
            }
            return;
        }
        self.doc.selection = None;
        if self.over_tree(x) {
            self.pane = Pane::Tree;
            // Under the panel's top border.
            let Some(line) = y.checked_sub(tree_top + 1) else {
                return;
            };
            match self.tree.click(line) {
                Some(Item::File(file)) => self.open(file),
                Some(Item::Dir { open }) => self.tree.set_open(!open),
                // A tap shows the commit's header and its files.
                Some(Item::Commit { file, open }) => {
                    if !open {
                        self.tree.set_open(true);
                    }
                    self.open(file);
                }
                None => {}
            }
            return;
        }
        self.pane = Pane::Diff;
        if x + 1 == self.screen.width() {
            self.doc.overview_click(y - 1);
            return;
        }
        self.tapped = self.code_at(x, y);
        self.doc.click(y - 1);
        let Some(hit) = x
            .checked_sub(self.content_x())
            .and_then(|column| self.doc.hit(y - 1, column, None))
        else {
            return;
        };
        // A double click selects the word, a triple click the line.
        if count > 1 && matches!(hit, Hit::Code { .. }) {
            self.doc.select_word(&hit, count == 3);
        }
        self.press = Some(Press {
            hit,
            at: (x, y),
            selected: count > 1,
        });
    }

    /// The pointer moved with the button held: a selection from where it
    /// went down to the cell under it, by characters from code, by lines
    /// from the line numbers, and on the side it started on. Held past the
    /// top or bottom of the pane, the view scrolls and the selection goes
    /// on, a line at a time.
    fn drag(&mut self, x: usize, y: usize) {
        let Some(press) = &mut self.press else {
            return;
        };
        if !press.selected {
            if (x, y) == press.at {
                return;
            }
            press.selected = true;
            self.doc.select_hit(&press.hit);
        }
        let Some(side) = self.doc.selection.as_ref().map(|s| s.side) else {
            return;
        };
        let pane = y.saturating_sub(1);
        if y == 0 || pane >= self.doc.height {
            self.drag_edge = Some((x, y));
            self.doc.select_by(if y == 0 { -1 } else { 1 });
            return;
        }
        self.drag_edge = None;
        let column = x.saturating_sub(self.content_x());
        if let Some(hit) = self.doc.hit(pane, column, Some(side)) {
            self.doc.select_to_hit(&hit);
        }
    }

    /// The button let go: what a drag or a double or triple click selected
    /// goes on the clipboard, and stays selected for `y` to copy again.
    fn release(&mut self) -> Option<Action> {
        self.drag_edge = None;
        let press = self.press.take()?;
        if press.selected && self.doc.selection.is_some() {
            return self.copy();
        }
        None
    }

    fn edit(&mut self) -> Option<Action> {
        let entry = self.doc.files.get(self.doc.pos.file)?;
        if entry.is_commit() {
            self.message("a commit has no file to open");
            return None;
        }
        let change = &entry.change;
        if change.new.is_none() {
            self.message("this file is deleted; there's nothing to open");
            return None;
        }
        let path = change.path.clone();
        let line = self.doc.line_at_top();
        Some(Action::Edit { path, line })
    }

    /// The search's scan of the file shown, once it's diffed and until its
    /// matches are in for the lines it shows now.
    fn find_job(&self) -> Option<Job> {
        let search = self.search.as_ref().filter(|s| !s.needle.text.is_empty())?;
        let file = self.doc.pos.file;
        let hidden = self.doc.hidden_lines(file)?;
        if self
            .doc
            .search
            .as_ref()
            .is_some_and(|m| m.file == file && m.hidden == hidden)
        {
            return None;
        }
        Some(Job::Find {
            file,
            version: self.version(file),
            diff: Arc::clone(self.doc.diff_of(file)?),
            hidden,
            needle: Arc::clone(&search.needle),
            id: search.id,
            live: Arc::clone(&self.live_search),
        })
    }

    /// Work for the pool, most wanted first: diffs of files on screen, the
    /// search's scan of the file shown, emphasis for hunks on screen,
    /// highlighting of revealed lines on screen, syntax diffs of files on
    /// screen when that view is on, diffs of nearby files, then the
    /// search's counts of the other files.
    pub(crate) fn jobs(&self) -> Vec<Job> {
        let files = &self.doc.files;
        let diff = |file: usize| {
            files[file].wants_diff().then(|| Job::Diff {
                file,
                version: files[file].version,
                change: files[file].change.clone(),
                piped: match &files[file].source {
                    Source::Piped(piped) => Some(Arc::clone(piped)),
                    Source::Repo | Source::Commit(_) => None,
                },
            })
        };
        let mut jobs: Vec<Job> = self.visible.clone().filter_map(diff).collect();
        jobs.extend(self.find_job());
        jobs.extend(
            self.doc
                .missing_emphasis
                .iter()
                .filter_map(|&(file, hunk)| match &files[file].state {
                    State::Loaded(loaded) => Some(Job::Emphasis {
                        file,
                        version: files[file].version,
                        hunk,
                        diff: Arc::clone(&loaded.diff),
                        syntax: loaded.syntax.clone(),
                    }),
                    _ => None,
                }),
        );
        jobs.extend(
            self.doc
                .missing_gap_syntax
                .iter()
                .filter_map(|&(file, gap)| match &files[file].state {
                    State::Loaded(loaded) => Some(Job::GapSyntax {
                        file,
                        version: files[file].version,
                        gap,
                        lines: loaded.gap_lines(gap),
                        diff: Arc::clone(&loaded.diff),
                        syntax: loaded.syntax.clone()?,
                    }),
                    _ => None,
                }),
        );
        jobs.extend(self.render.as_ref().map(|target| {
            Job::Render(RenderJob {
                file: target.file,
                version: self.version(target.file),
                diff: Arc::clone(&target.diff),
                program: target.program.clone(),
                width: target.width,
                light: target.light,
                id: self.render_id,
                live: Arc::clone(&self.live_render),
            })
        }));
        if self.doc.structural {
            jobs.extend(
                self.visible
                    .clone()
                    .filter_map(|file| match &files.get(file)?.state {
                        State::Loaded(loaded) if loaded.wants_structural() => {
                            Some(Job::Structural {
                                file,
                                version: files[file].version,
                                diff: Arc::clone(&loaded.diff),
                            })
                        }
                        _ => None,
                    }),
            );
        }
        let ahead = self.visible.end..(self.visible.end + PREFETCH_AHEAD).min(files.len());
        let behind = self.visible.start.saturating_sub(PREFETCH_BEHIND)..self.visible.start;
        jobs.extend(ahead.chain(behind).filter_map(diff));
        jobs.extend(self.search_jobs());
        jobs
    }

    /// The search's counts of the files not shown, nearest after the one
    /// shown first.
    fn search_jobs(&self) -> Vec<Job> {
        let Some(search) = self.search.as_ref().filter(|s| !s.needle.text.is_empty()) else {
            return Vec::new();
        };
        let files = &self.doc.files;
        let n = files.len();
        let start = self.doc.pos.file;
        (1..n)
            .map(|k| (start + k) % n)
            .filter(|&f| !files[f].is_commit() && search.counts.get(f).is_none_or(Option::is_none))
            .take(SEARCH_AHEAD)
            .map(|file| Job::Search {
                file,
                version: files[file].version,
                change: files[file].change.clone(),
                piped: match &files[file].source {
                    Source::Piped(piped) => Some(Arc::clone(piped)),
                    Source::Repo | Source::Commit(_) => None,
                },
                needle: Arc::clone(&search.needle),
                id: search.id,
                live: Arc::clone(&self.live_search),
            })
            .collect()
    }

    /// What an empty diff pane says: as printed for an empty diff, and
    /// whether it's watching for changes.
    fn empty_note(&self) -> String {
        if self.enumerating {
            return "listing changes…".to_owned();
        }
        let text = match &self.selected {
            Some((revisions, default)) => print::empty_text(revisions, *default, self.others),
            None => "no changes".to_owned(),
        };
        if self.watch == Watch::Off {
            text
        } else {
            format!("{text} — watching")
        }
    }

    /// The hunk or function the view is in, for the title: `@@ fn name()`,
    /// the hunk's numbers when it has no heading, or with hunk headers off
    /// the heading alone.
    fn context(&mut self) -> Option<chrome::Spans> {
        let context = self.doc.context()?;
        match context.numbers {
            Some(numbers) if self.hunk_headers && context.heading.is_empty() => {
                Some(vec![(numbers, Role::HunkHeader)])
            }
            Some(_) if self.hunk_headers => Some(vec![
                ("@@ ".to_owned(), Role::HunkHeader),
                (context.heading, Role::Dim),
            ]),
            _ if context.heading.is_empty() => None,
            _ => Some(vec![(context.heading, Role::Dim)]),
        }
    }

    fn chrome(&mut self) -> Chrome {
        let narrow = !self.side_tree();
        let note = note(self);
        let doc = &self.doc;
        let extra = doc
            .files
            .get(doc.pos.file)
            .and_then(|e| match &e.state {
                State::Loaded(l) if l.diff.note.is_some() => l.diff.note.clone(),
                State::Loaded(l) if doc.structural => l.structural_note().map(str::to_owned),
                _ => None,
            })
            .or_else(|| self.render_note());
        let status = chrome::Status {
            chip: chip(self),
            position: position(self).filter(|_| !narrow),
            hunk: doc.hunk_position().map(|(i, n)| format!("{i}/{n} hunk")),
            middle: note.clone().or_else(|| Some(modes(self))),
            note: note.is_some(),
            search: [
                self.search_status(),
                (!self.tree.filter().is_empty()).then(|| "filter".to_owned()),
            ]
            .into_iter()
            .flatten()
            .reduce(|a, b| format!("{a} {b}")),
            extra,
            hints: hints(self, narrow),
        };
        // The picker covers the tree, which isn't drawn under it.
        let tree = self.picker.is_none();
        Chrome {
            framed: self.framed(),
            status_top: self.status_bar == StatusBar::Top,
            tree: self.side_width().filter(|_| tree),
            overlay: self.tree_shown && narrow && tree,
            tree_editing: self.tree.editing,
            heading: heading(self),
            context: self.context(),
            status,
        }
    }

    /// The bytes for the terminal when something changed since the last
    /// frame.
    pub(crate) fn draw(&mut self) -> Option<&[u8]> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        self.frame += 1;
        self.update_render();
        let (width, height) = (self.screen.width(), self.screen.height());
        let x = self.content_x();
        let tree_columns = self.tree_columns();
        let framed = self.framed();
        let search_line = self.search.as_ref().filter(|s| s.editing).map(|s| {
            let file = self.doc.pos.file;
            let here = match &self.doc.search {
                Some(m) if m.file == file => m.matches.list.len().to_string(),
                _ if s.needle.text.is_empty() => "0".to_owned(),
                _ => "…".to_owned(),
            };
            (
                format!("/{}", s.needle.text),
                format!("  {here} in this file · enter goes · esc cancels"),
            )
        });
        let entries = self.commands.as_ref().map(|_| self.command_entries());
        let chrome = self.chrome();
        let empty = self.doc.files.is_empty().then(|| self.empty_note());
        let overlays = self.picker.is_some() || self.help.is_some() || self.commands.is_some();
        let (tree_top, tree_rows) = self.tree_rows();
        let picker_top = self.picker_top();
        let grid = self.screen.frame();
        chrome.outline(grid, &self.theme);
        self.visible = self.doc.draw(grid, x, width, 1, &self.theme, self.frame);
        // The picker covers the tree, and refreshing it costs a pass over
        // every file while the picker reads a long input.
        if let Some(columns) = tree_columns.filter(|_| self.picker.is_none()) {
            let files = &self.doc.files;
            if self.pane == Pane::Diff && !files.is_empty() {
                self.tree.follow(self.doc.pos.file);
            }
            let totals = Totals::of(files, self.enumerating);
            if chrome.overlay {
                // Blank out the diff under it.
                for y in tree_top..tree_top + tree_rows {
                    grid.fill(0, y, columns, Style::PLAIN);
                }
            }
            self.tree.draw(
                grid,
                (tree_top, tree_rows, columns),
                files,
                &totals,
                self.enumerating,
                self.pane == Pane::Tree,
                framed && !chrome.overlay,
                &self.theme,
            );
        }
        // After the tree, so an empty one over the diff says why.
        if let Some(text) = empty {
            let left = x + width.saturating_sub(x + text.width()) / 2;
            grid.put_str(left, height / 2, width, &text, self.theme.style(Role::Dim));
        }
        self.switcher = chrome.text(grid, &self.theme);
        self.tree_button = (!overlays)
            .then(|| {
                tree_button(
                    grid,
                    tree_columns,
                    (framed, tree_top + tree_rows),
                    &self.theme,
                )
            })
            .flatten();
        if let Some(line) = &search_line {
            draw_search_line(grid, line, framed, &self.theme);
        }
        if let Some(picker) = &mut self.picker {
            let progress = if !self.enumerating {
                picker::Progress::Done
            } else if self.doc.files.len() >= piped::MAX_ENTRIES {
                picker::Progress::Capped
            } else {
                picker::Progress::Reading
            };
            draw_picker(grid, picker, &self.doc, progress, picker_top, &self.theme);
        }
        if let (Some(commands), Some(entries)) = (&mut self.commands, entries) {
            commands.draw(grid, &self.theme, &entries);
        }
        if let Some(help) = &mut self.help {
            help.draw(grid, &self.theme, &self.keys);
        }
        Some(self.screen.flush())
    }
}

/// `«` on the tree's bottom border (whose row `tree_end` is after), to
/// hide it, or with no tree, `»` at the bottom left, on the frame's border
/// or the bottom row, to show it: for a mouse or a tap. Returns where it
/// went.
fn tree_button(
    grid: &mut term::Grid,
    tree_columns: Option<usize>,
    (framed, tree_end): (bool, usize),
    theme: &Theme,
) -> Option<(usize, usize)> {
    let (width, height) = (grid.width(), grid.height());
    if height < 3 {
        return None;
    }
    let (glyph, at) = match tree_columns {
        Some(columns) if columns >= 4 => ("«", (1, tree_end.checked_sub(1)?)),
        Some(_) => return None,
        None => ("»", (usize::from(framed), height - 1)),
    };
    grid.put_str(at.0, at.1, width, glyph, theme.style(Role::Hint));
    Some(at)
}

/// The search being typed, on the bottom row, inside the frame's corners:
/// `/pattern▏` and a note.
fn draw_search_line(
    grid: &mut term::Grid,
    (typed, note): &(String, String),
    framed: bool,
    theme: &Theme,
) {
    let (width, height) = (grid.width(), grid.height());
    if height < 2 {
        return;
    }
    let (y, inset) = (height - 1, usize::from(framed));
    let end = width.saturating_sub(inset);
    grid.fill(inset, y, end, Style::PLAIN);
    let after = grid.put_str(inset + inset, y, end, typed, theme.style(Role::Hint));
    let after = grid.put_str(after, y, end, "▏", theme.style(Role::Hint));
    grid.put_str(after, y, end, note, theme.style(Role::Dim));
}

/// The picker over every row from `top` but the status row.
fn draw_picker(
    grid: &mut term::Grid,
    picker: &mut Picker,
    doc: &Doc,
    progress: picker::Progress,
    top: usize,
    theme: &Theme,
) {
    let (width, height) = (grid.width(), grid.height());
    let rows_shown = height.saturating_sub(1);
    picker.catch_up(&doc.files);
    let rows = picker.rows(&doc.files, &doc.commits, (width, rows_shown), progress);
    for y in top..top + rows_shown {
        grid.fill(0, y, width, Style::PLAIN);
    }
    for (i, (row, selected)) in rows.iter().enumerate() {
        if let Some(cells) = row.rows().next() {
            view::paint(grid, 0, top + i, width, cells, theme, Attributes::NONE);
        }
        if *selected {
            grid.map_styles(0, top + i, width, |s| theme.selected(s, true));
        }
    }
}

/// The view modes for the status bar: `split syntax wrap watch`.
fn modes(app: &App) -> String {
    let doc = &app.doc;
    let mut modes = String::new();
    modes.push_str(if doc.layout.split { "split" } else { "unified" });
    if doc.structural {
        modes.push_str(" syntax");
    }
    // Only on files it applies to: elsewhere it would be noise.
    let renders = doc.files.get(doc.pos.file).is_some_and(|e| {
        matches!(e.source, Source::Repo) && app.renderers.for_path(&e.change.path).is_some()
    });
    if doc.rendered && renders {
        modes.push_str(" rendered");
    }
    if doc.files.get(doc.pos.file).is_some_and(|e| doc.is_full(e)) {
        modes.push_str(" full");
    }
    if app.wrap {
        modes.push_str(" wrap");
    }
    modes.push_str(match app.watch {
        Watch::Off => "",
        Watch::Starting | Watch::Watching => " watch",
        Watch::Polling => " watch:polling",
    });
    if !app.mouse {
        let key = app
            .keys
            .chords(false, Act::ToggleMouse)
            .next()
            .map(|c| format!(" ({})", c.name()))
            .unwrap_or_default();
        let _ = write!(modes, " mouse off{key}");
    }
    modes
}

/// What's compared, for the status bar's chip; under the commits of a
/// piped log or several, which commit of how many.
fn chip(app: &App) -> String {
    let doc = &app.doc;
    let growing = if app.enumerating { "+" } else { "" };
    match doc.commit_of(doc.pos.file) {
        Some(c) if doc.commits.len() > 1 || app.piped => {
            format!("commit {}/{}{growing}", c + 1, doc.commits.len())
        }
        _ => app.description.clone(),
    }
}

/// Which file of how many, or under a commit, which of its files (on the
/// commit itself, how many it has). A `+` marks counts still growing.
fn position(app: &App) -> Option<String> {
    let doc = &app.doc;
    if doc.files.is_empty() {
        return None;
    }
    let growing = if app.enumerating { "+" } else { "" };
    let file = doc.pos.file;
    let Some(c) = doc.commit_of(file) else {
        let total = doc.files.len();
        return Some(format!("{}/{total}{growing}", (file + 1).min(total)));
    };
    let entry = doc.commits[c];
    let files = doc.commit_files(entry).len();
    let last = if c + 1 == doc.commits.len() {
        growing
    } else {
        ""
    };
    Some(if file == entry {
        let s = if files == 1 { "" } else { "s" };
        format!("{files}{last} file{s}")
    } else {
        format!("{}/{files}{last}", file - entry)
    })
}

/// The narrow switcher's position: the file of how many, under commits
/// the file within its commit, and on a commit the commit of how many.
fn switcher_position(app: &App) -> String {
    let doc = &app.doc;
    let growing = if app.enumerating { "+" } else { "" };
    let file = doc.pos.file;
    match doc.commit_of(file) {
        Some(c) if doc.commits[c] == file => {
            format!("{}/{}{growing}", c + 1, doc.commits.len())
        }
        Some(_) => position(app).unwrap_or_default(),
        None => {
            let total = doc.files.len();
            format!("{}/{total}{growing}", (file + 1).min(total))
        }
    }
}

/// The file shown, for the title: on wide screens its state, staging,
/// path and counts (a commit's id and subject), on narrow ones the file
/// switcher.
fn heading(app: &App) -> Option<Heading> {
    let doc = &app.doc;
    let entry = doc.files.get(doc.pos.file)?;
    let commit = entry.is_commit();
    let counts = entry.counts.filter(|_| !commit);
    let staging = entry
        .change
        .staging
        .filter(|_| !commit && entry.change.status != Status::Untracked)
        .map(crate::render::stat::staging_word);
    let path = escape(&entry.change.path);
    let (dir, name) = match path.rfind('/') {
        Some(i) => (path[..=i].to_owned(), path[i + 1..].to_owned()),
        None => (String::new(), path.clone()),
    };
    if !app.side_tree() {
        return Some(Heading::Switcher(Switcher {
            position: switcher_position(app),
            name: if commit { path } else { name },
            counts: counts.map(|n| (n.added, n.removed)),
            staging,
        }));
    }
    let dim = |text: &str| (text.to_owned(), Role::Dim);
    if let Source::Commit(c) = &entry.source {
        let (head, subject) = match &c.id {
            Some(id) => (
                vec![
                    ("commit ".to_owned(), Role::Plain),
                    (id[..id.len().min(7)].to_owned(), Role::CommitHeader),
                    dim(" · "),
                ],
                c.subject.clone(),
            ),
            None => (Vec::new(), path),
        };
        return Some(Heading::File(Title {
            head,
            flex: (subject, Role::Plain, Cut::Right),
            name: (String::new(), Role::Plain),
            tail: Vec::new(),
        }));
    }
    let mut head = vec![(entry.status().word().to_owned(), Role::Plain)];
    if let Some(staging) = staging {
        head.push((format!(" · {staging}"), Role::Plain));
    }
    head.push(dim(" · "));
    let flex = match &entry.change.status {
        Status::Renamed { from, .. } => format!("{} → {dir}", escape(from)),
        _ => dir,
    };
    let tail = counts.map_or_else(Vec::new, |n| {
        vec![
            dim(" · "),
            (format!("+{}", n.added), Role::Added),
            (format!(" -{}", n.removed), Role::Removed),
        ]
    });
    Some(Heading::File(Title {
        head,
        flex: (flex, Role::Dim, Cut::Left),
        name: (name, Role::FileHeader),
        tail,
    }))
}

/// A note for the status bar, in place of the mode words: the message,
/// or while lines are selected, how many.
fn note(app: &App) -> Option<String> {
    app.message
        .as_ref()
        .map(|(text, _)| text.clone())
        .or_else(|| {
            let selection = app.doc.selection.as_ref()?;
            let lines = selection.len();
            let s = if lines == 1 { "" } else { "s" };
            let side = match selection.side {
                Side::Old => "old",
                Side::New => "new",
            };
            let copy = app.keys.chords(false, Act::Copy).next()?.name();
            Some(format!("{lines} {side} line{s} · {copy} copies"))
        })
}

/// The status bar's key hints for the focused pane, longest first:
/// `/ search · tab tree · space commands` with the diff focused, `/ filter ·
/// tab diff · space commands` with the tree; then without the pane switch;
/// then the menu alone. Each key is the first one that runs its action
/// where the focus is, and a part whose action has none is left out. On a
/// narrow screen `space` shows as `␣`.
fn hints(app: &App, narrow: bool) -> [Option<String>; 3] {
    let tree = app.pane == Pane::Tree;
    let key = |action| {
        app.keys
            .chords(tree, action)
            .chain(app.keys.chords(false, action))
            .find(|c| app.keys.action(tree, c.key, c.mods) == Some(action))
            .map(crate::keys::Chord::name)
    };
    let (find, other) = if tree {
        (key(Act::FilterTree).map(|k| format!("{k} filter")), "diff")
    } else {
        (key(Act::Search).map(|k| format!("{k} search")), "tree")
    };
    let switch = key(Act::SwitchPane).map(|k| format!("{k} {other}"));
    let menu = key(Act::Leader).map(|k| {
        let k = if narrow && k == "space" {
            "␣".to_owned()
        } else {
            k
        };
        format!("{k} commands")
    });
    let join = |parts: &[&Option<String>]| {
        let parts: Vec<&str> = parts.iter().filter_map(|p| p.as_deref()).collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    };
    [join(&[&find, &switch, &menu]), join(&[&find, &menu]), menu]
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;

    use dffy_core::diff::{Options, Text, Whitespace, hunks};
    use dffy_core::git::{ChangeSet, FileDiff, Mode, Repo, Side};

    use super::*;
    use crate::tui::view::Loaded;

    fn diff(path: &str) -> FileDiff {
        let side = Some(Side::Worktree {
            mode: Mode::File,
            id: None,
        });
        let old = (1..=60)
            .map(|i| format!("line {i}\n"))
            .collect::<Vec<_>>()
            .concat();
        let new = old
            .replace("line 4\n", "line four\n")
            .replace("line 50\n", "line fifty\n");
        let (old, new) = (Text::new(old.into()), Text::new(new.into()));
        FileDiff {
            change: FileChange {
                path: path.into(),
                status: Status::Modified,
                old: side,
                new: side,
                staging: None,
            },
            hunks: hunks(&old, &new, &Options::default()),
            old,
            new,
            binary: false,
            too_large: false,
            note: None,
        }
    }

    /// A `width` × `height` viewer of two files, the first diffed.
    fn app(width: usize, height: usize) -> App {
        let first = diff("src/some/deeply/nested/module/file.rs");
        app_of(&Config::default(), "working tree", first, (width, height))
    }

    /// A viewer of `first`, diffed, and a second file, comparing what
    /// `description` says.
    fn app_of(config: &Config, description: &str, first: FileDiff, size: (usize, usize)) -> App {
        let mut app = App::new(
            config,
            Theme::default_theme(),
            description.to_owned(),
            Some(first.change.clone()),
            size,
            Indicators::Classic,
        );
        app.event(Event::Enumerated(diff("src/zz.rs").change));
        app.event(Event::EnumerationDone(None));
        let loaded = Loaded::new(first, Whitespace::Exact, None);
        app.event(Event::Diffed(Done::Diff {
            file: 0,
            version: app.version(0),
            result: Ok(Box::new(loaded)),
        }));
        app
    }

    fn screen(app: &mut App) -> String {
        app.draw();
        app.screen.text()
    }

    fn key(app: &mut App, c: char) {
        app.event(Event::Input(Input::Key(Key::Char(c), Mods::NONE)));
    }

    /// A new file of `lines` lines: one whole-file hunk.
    fn added(lines: usize) -> FileDiff {
        let new = (1..=lines)
            .map(|i| format!("line {i}\n"))
            .collect::<Vec<_>>()
            .concat();
        let (old, new) = (Text::new(Vec::new()), Text::new(new.into()));
        FileDiff {
            change: FileChange {
                path: "notes.txt".into(),
                status: Status::Added,
                old: None,
                new: Some(Side::Worktree {
                    mode: Mode::File,
                    id: None,
                }),
                staging: None,
            },
            hunks: hunks(&old, &new, &Options::default()),
            old,
            new,
            binary: false,
            too_large: false,
            note: None,
        }
    }

    /// The file is diffed again while emphasis for the diff still shown is
    /// being computed, and the new diff lands first: that emphasis was
    /// computed for the old hunk and must not be drawn with the new one.
    /// The pool held back the new hunk's job while the old one ran, so
    /// dropping the old result draws again to ask for it.
    #[test]
    fn emphasis_for_a_diff_since_replaced_is_dropped() {
        let mut app = app_of(&Config::default(), "working tree", added(46), (187, 80));
        screen(&mut app);
        // Diffing again: the old diff stays shown until the new one lands.
        key(&mut app, ' ');
        key(&mut app, 'W');
        screen(&mut app);
        let jobs = app.jobs();
        let Some(Job::Emphasis {
            file,
            version,
            hunk,
            diff,
            ..
        }) = jobs.into_iter().find(|j| matches!(j, Job::Emphasis { .. }))
        else {
            panic!("emphasis for the hunk shown");
        };
        assert_eq!(diff.new.len(), 46);
        app.event(Event::Diffed(Done::Diff {
            file,
            version,
            result: Ok(Box::new(Loaded::new(added(60), Whitespace::Exact, None))),
        }));
        assert!(screen(&mut app).contains("line 60"));
        app.event(Event::Diffed(Done::Emphasis {
            file,
            version,
            hunk,
            emphasis: dffy_core::diff::emphasize(&diff.old, &diff.new, &diff.hunks[hunk]),
            syntax: None,
            diff,
        }));
        assert!(app.draw().is_some());
        let current = app.doc.diff_of(file).map(Arc::clone);
        assert!(app.jobs().iter().any(|j| matches!(
            j,
            Job::Emphasis { diff, .. } if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, diff))
        )));
    }

    /// A watch refresh voids the work of the files it touched and no
    /// other: under constant writes to one file, another's emphasis still
    /// lands, and a diff of the written file taken before the write is
    /// never shown as current.
    #[test]
    fn a_refresh_of_one_file_keeps_the_work_for_the_others() {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(d)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "A")
                .env("GIT_AUTHOR_EMAIL", "a@example.com")
                .env("GIT_COMMITTER_NAME", "A")
                .env("GIT_COMMITTER_EMAIL", "a@example.com")
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(d.join("a.txt"), "a\n").expect("write");
        std::fs::write(d.join("b.txt"), "one\ntwo\n").expect("write");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "x"]);
        std::fs::write(d.join("a.txt"), "a2\n").expect("write");
        std::fs::write(d.join("b.txt"), "one\nTWO\n").expect("write");

        let repo = Repo::discover(d).expect("repo");
        let spec = repo.resolve(&[], false, false).expect("spec");
        let mut set = ChangeSet::new(repo.clone(), spec, Revisions::default(), &[], false);
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "working tree".to_owned(),
            None,
            (100, 20),
            Indicators::Classic,
        );
        set.enumerate(|change| {
            app.event(Event::Enumerated(change));
            ControlFlow::Continue(())
        })
        .expect("enumerate");
        app.event(Event::EnumerationDone(None));
        let diffed = |app: &App, file: usize| {
            let change = app.doc.files[file].change.clone();
            let fd = repo.file_diff(change, &Options::default()).expect("diff");
            Done::Diff {
                file,
                version: app.version(file),
                result: Ok(Box::new(Loaded::new(fd, Whitespace::Exact, None))),
            }
        };
        for file in [0, 1] {
            let done = diffed(&app, file);
            app.event(Event::Diffed(done));
        }
        key(&mut app, 'f');
        assert_eq!(app.doc.files[app.doc.pos.file].change.path, "b.txt");
        app.draw();
        let emphasis = app
            .jobs()
            .into_iter()
            .find_map(|job| match job {
                Job::Emphasis {
                    file,
                    version,
                    hunk,
                    diff,
                    ..
                } => Some(Done::Emphasis {
                    file,
                    version,
                    hunk,
                    emphasis: dffy_core::diff::emphasize(&diff.old, &diff.new, &diff.hunks[hunk]),
                    syntax: None,
                    diff,
                }),
                _ => None,
            })
            .expect("emphasis for b.txt's hunk");
        let before_the_write = diffed(&app, 0);

        std::fs::write(d.join("a.txt"), "a3\n").expect("write");
        let update = set
            .refresh(&["a.txt".into()])
            .expect("refresh")
            .expect("changed");
        app.event(Event::Refreshed(update));
        app.event(Event::Diffed(emphasis));
        app.event(Event::Diffed(before_the_write));

        assert!(app.draw().is_some());
        assert!(
            !app.jobs().iter().any(|j| matches!(j, Job::Emphasis { .. })),
            "b.txt's emphasis was kept"
        );
        assert!(
            app.doc.files[0].wants_diff(),
            "a.txt's diff from before the write isn't taken"
        );
    }

    #[test]
    fn a_phone_sized_screen_compacts_the_status_line_and_overlays_the_tree() {
        let mut app = app(40, 12);
        insta::assert_snapshot!("narrow", screen(&mut app));
        app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
        insta::assert_snapshot!("narrow_tree", screen(&mut app));
        // Picking a file closes the overlay.
        key(&mut app, 'j');
        app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
        let shown = screen(&mut app);
        assert!(!shown.contains("┌ Files"), "{shown}");
        assert!(
            shown.lines().next().is_some_and(|l| l.contains("2/2")),
            "{shown}"
        );
    }

    /// The screen with each run of the status chip's cells in `[…]`.
    fn chip_markup(app: &mut App) -> String {
        app.draw();
        let tag = app.theme.style(Role::StatusTag);
        let grid = app.screen.shown_grid();
        (0..grid.height())
            .map(|y| {
                let mut line = String::new();
                let mut on = false;
                for cell in grid.row(y) {
                    let now = cell.style() == tag;
                    if now != on {
                        line.push(if now { '[' } else { ']' });
                        on = now;
                    }
                    line.push_str(cell.text());
                }
                line.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One frame around the tree and the diff: the top border holds the
    /// tree's totals, the file's title and the hunk the view is in; the
    /// bottom border is the status bar. The divider joins both with `┬`
    /// and `┴`.
    #[test]
    fn the_frame_holds_the_title_context_and_status() {
        let mut app = app(120, 18);
        insta::assert_snapshot!("frame_wide", chip_markup(&mut app));
        // Without the tree, the frame goes round the diff alone; scrolled
        // into the second hunk, the context follows.
        key(&mut app, 't');
        key(&mut app, 'n');
        key(&mut app, 'n');
        insta::assert_snapshot!("frame_no_tree", chip_markup(&mut app));
    }

    /// A file changed inside a function named on its first line, the
    /// heading of its one hunk.
    fn heading_diff() -> FileDiff {
        let old: String =
            std::iter::once("fn render_the_frame_and_its_status_bar() {\n".to_owned())
                .chain((1..=40).map(|i| format!("    let line_{i} = {i};\n")))
                .chain(["}\n".to_owned()])
                .collect();
        let new = old.replace("let line_20 = 20;", "let line_20 = 21;");
        let side = Some(Side::Worktree {
            mode: Mode::File,
            id: None,
        });
        let (old, new) = (Text::new(old.into()), Text::new(new.into()));
        FileDiff {
            change: FileChange {
                path: "src/some/deeply/nested/module/file.rs".into(),
                status: Status::Modified,
                old: side,
                new: side,
                staging: None,
            },
            hunks: hunks(&old, &new, &Options::default()),
            old,
            new,
            binary: false,
            too_large: false,
            note: None,
        }
    }

    /// Short of room, the context shrinks first, then the directory, cut
    /// from the left; the name and counts stay.
    #[test]
    fn the_title_gives_up_the_context_then_the_directory() {
        let titles: Vec<String> = [130, 100, 72, 60, 48]
            .into_iter()
            .map(|diff_width| {
                // The diff's part of the top border, the tree taking the
                // rest of the screen; in the hunk, its whole text shown.
                let mut app = app_of(&Config::default(), "uncommitted", heading_diff(), (160, 18));
                app.tree_width = Some(160 - diff_width);
                app.relayout();
                key(&mut app, 'X');
                key(&mut app, 'n');
                let shown = screen(&mut app);
                let top = shown.lines().next().unwrap_or_default();
                top.chars().skip(160 - diff_width - 1).collect::<String>()
            })
            .collect();
        insta::assert_snapshot!(titles.join("\n"), @r"
        ┬─ modified · src/some/deeply/nested/module/file.rs · +1 -1 ────────────────────── @@ fn render_the_frame_and_its_status_bar() { ─┐
        ┬─ modified · src/some/deeply/nested/module/file.rs · +1 -1 ─ @@ fn render_the_frame_and_its_stat… ─┐
        ┬─ modified · src/some/deeply/nested/module/file.rs · +1 -1 ────────────┐
        ┬─ modified · …c/some/deeply/nested/module/file.rs · +1 -1 ─┐
        ┬─ modified · …y/nested/module/file.rs · +1 -1 ─┐
        ");
    }

    #[test]
    fn a_note_replaces_the_mode_words() {
        let mut app = app(120, 18);
        key(&mut app, 'W');
        let shown = screen(&mut app);
        let status = shown.lines().last().unwrap_or_default();
        assert!(status.contains("wrap"), "{status}");
        app.message("copied src/some/deeply/nested/module/file.rs");
        insta::assert_snapshot!(screen(&mut app).lines().last().unwrap_or_default(), @"└«─  working tree  ─────────────┴─ 1/2 · 1/2 hunk  copied src/some/deeply/nested/module/file.rs ────── space commands ─┘");
    }

    /// Under 16 rows there's no frame: a title row, then code, then the
    /// status row.
    #[test]
    fn a_short_screen_has_a_title_row_and_a_status_row() {
        let mut app = app(50, 15);
        insta::assert_snapshot!("short", chip_markup(&mut app));
    }

    /// `status-bar = top` puts the status in the top border after the
    /// title, and in the toggle's state remembered.
    #[test]
    fn the_status_bar_can_go_on_top() {
        let mut config = Config::default();
        config.apply_source("status-bar = top\n", &mut Vec::new());
        let mut app = app_of(&config, "v6.0..v6.1", heading_diff(), (160, 16));
        key(&mut app, 'X');
        key(&mut app, 'n');
        insta::assert_snapshot!("status_top", chip_markup(&mut app));
        key(&mut app, ' ');
        key(&mut app, 'b');
        assert_eq!(app.status_bar, StatusBar::Bottom);
        assert_eq!(
            app.take_remembered(true),
            [
                ("context", "full".to_owned()),
                ("status-bar", "bottom".to_owned())
            ]
        );
    }

    /// At 40 columns a long chip keeps its own cells: what doesn't fit
    /// after it goes, rather than drawing over it.
    #[test]
    fn a_long_chip_on_a_phone_is_never_overdrawn() {
        let mut app = app_of(
            &Config::default(),
            "origin/main...HEAD",
            diff("src/a.rs"),
            (40, 12),
        );
        let shown = screen(&mut app);
        let status = shown.lines().last().unwrap_or_default();
        assert!(status.contains(" origin/main...HEAD "), "{status}");
        assert!(status.chars().count() <= 40, "{status}");
    }

    /// The picker covers the tree, so the frame's edges go straight across
    /// while it's open.
    #[test]
    fn the_frame_loses_the_tree_under_the_picker() {
        let mut app = app(200, 60);
        key(&mut app, 'o');
        let shown = screen(&mut app);
        let status = shown.lines().last().unwrap_or_default();
        assert!(status.starts_with("└──"), "{status}");
        assert!(!status.contains('┴'), "{status}");
    }

    /// A split remembered from an earlier run shows unified on a phone and
    /// stays the saved choice; split asked for in this run (a flag, the
    /// config) splits there, down to the per-hunk fallback.
    #[test]
    fn a_remembered_split_shows_unified_on_a_phone() {
        let config = Config {
            view: View::Split,
            ..Config::default()
        };
        let phone = |remembered: bool| {
            let mut app = App::new(
                &config,
                Theme::default_theme(),
                "uncommitted".to_owned(),
                Some(diff("a.rs").change),
                (50, 20),
                Indicators::Classic,
            );
            app.set_view_remembered(remembered);
            app
        };
        let mut app = phone(true);
        assert!(!app.doc.layout.split);
        assert_eq!(app.view, View::Split);
        assert!(
            app.take_remembered(true).is_empty(),
            "the saved choice stands"
        );
        app.resize(160, 20);
        assert!(app.doc.layout.split, "split again on a wide screen");
        assert!(phone(false).doc.layout.split);
        // A choice made here is this run's: `s` round to split splits.
        let mut app = phone(true);
        for _ in 0..3 {
            key(&mut app, 's');
        }
        assert_eq!(app.view, View::Split);
        assert!(app.doc.layout.split);
    }

    /// The chrome takes two rows whatever the screen: the frame's borders,
    /// or a title row and a status row; every other row is code.
    #[test]
    fn the_chrome_takes_two_rows() {
        for (width, height) in [(200, 60), (50, 15)] {
            let mut app = app(width, height);
            key(&mut app, 'X');
            let shown = screen(&mut app);
            assert_eq!(app.doc.height, height - 2, "{width}×{height}");
            let code = (0..app.doc.height)
                .filter(|&y| app.doc.row_at(y).is_some())
                .count();
            assert_eq!(code, height - 2, "{width}×{height}:\n{shown}");
        }
    }

    #[test]
    fn every_key_works_at_forty_by_twelve() {
        let mut app = app(40, 12);
        for c in "nnNNffFFxXXjkgG sbW?q?TT/ab\u{7f}vjjskyYv<>HL0WvG+-".chars() {
            key(&mut app, c);
            app.draw();
        }
        for input in [
            Input::Click { x: 10, y: 5 },
            Input::Drag { x: 10, y: 8 },
            Input::Drag { x: 10, y: 0 },
            Input::Drag { x: 10, y: 11 },
            Input::Release { x: 10, y: 11 },
        ] {
            app.event(Event::Input(input));
            app.draw();
        }
        for key in [
            Key::Tab,
            Key::Esc,
            Key::Down,
            Key::Enter,
            Key::Tab,
            Key::PageDown,
        ] {
            app.event(Event::Input(Input::Key(key, Mods::NONE)));
            app.draw();
        }
        app.resize(20, 5);
        app.draw();
        app.resize(1, 1);
        app.draw();
    }

    /// The screen as text, the command panel's dimmed text as `<dim>…</>`.
    fn panel_markup(app: &mut App) -> String {
        app.draw();
        let dim = app.theme.style(Role::Dim);
        let grid = app.screen.shown_grid();
        (0..grid.height())
            .map(|y| {
                let mut line = String::new();
                let mut in_dim = false;
                for cell in grid.row(y) {
                    let dimmed = cell.style() == dim && cell.text() != " ";
                    if dimmed != in_dim {
                        line.push_str(if dimmed { "<dim>" } else { "</>" });
                        in_dim = dimmed;
                    }
                    line.push_str(cell.text());
                }
                if in_dim {
                    line.push_str("</>");
                }
                line.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn esc(app: &mut App) {
        app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
    }

    /// `space` opens the command panel: groups, a row per entry with its
    /// state, its key and, dimmed, the direct key when that's another.
    #[test]
    fn the_command_panel_lists_entries_with_states_and_keys() {
        let mut phone = app(50, 30);
        let mut app = app(140, 40);
        key(&mut app, ' ');
        assert!(app.commands.is_some());
        insta::assert_snapshot!("commands_wide", panel_markup(&mut app));
        key(&mut phone, ' ');
        insta::assert_snapshot!("commands_narrow", screen(&mut phone));
    }

    /// A row's key runs it at once; a toggle keeps the panel open with its
    /// box flipped, somewhere to go (the picker) closes it. `esc` closes.
    #[test]
    fn a_rows_key_runs_it_and_toggles_keep_the_panel_open() {
        let mut app = app(140, 40);
        key(&mut app, ' ');
        key(&mut app, 'w');
        assert!(app.wrap);
        assert!(app.commands.is_some(), "a toggle keeps it open");
        let shown = screen(&mut app);
        assert!(shown.contains("[x] Wrap long lines"), "{shown}");
        assert!(shown.lines().last().is_some_and(|l| l.contains("wrap on")));
        key(&mut app, 'o');
        assert!(app.commands.is_none(), "the picker closes it");
        assert!(app.picker.is_some());
        esc(&mut app);
        key(&mut app, ' ');
        key(&mut app, 'z');
        assert!(app.commands.is_some(), "a key with no entry keeps it open");
        assert!(status(&mut app).contains("no command"));
        esc(&mut app);
        assert!(app.commands.is_none());
    }

    /// Browsing: `j`/`k` move a cursor that goes round at the ends; `enter`
    /// runs the row under it; `left`/`h` step a setting back, `right`/`l`
    /// forward, and so does a tap on its `‹` or `›`.
    #[test]
    fn browsing_moves_a_cursor_and_enter_runs_its_row() {
        let mut app = app(140, 40);
        key(&mut app, ' ');
        let entries = app.command_entries().len();
        let cursor = |app: &App| app.commands.as_ref().map(Commands::cursor);
        let under = |app: &App| {
            let commands = app.commands.as_ref()?;
            commands.entry(&app.command_entries(), commands.cursor())
        };
        // Up from the first row is the last.
        key(&mut app, 'k');
        assert_eq!(cursor(&app), Some(entries - 1));
        key(&mut app, 'j');
        assert_eq!(cursor(&app), Some(0));
        // To the wrap row, then enter.
        for _ in 0..entries {
            if under(&app) == Some(Act::ToggleWrap) {
                break;
            }
            key(&mut app, 'j');
        }
        assert_eq!(under(&app), Some(Act::ToggleWrap));
        app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
        assert!(app.wrap);
        assert!(app.commands.is_some());
        assert!(screen(&mut app).contains("[x] Wrap long lines"));
        // The layout: auto to split by its key, which puts the cursor on
        // it, then back to auto.
        key(&mut app, 's');
        assert_eq!(app.view, View::Split);
        assert_eq!(under(&app), Some(Act::CycleLayout));
        app.event(Event::Input(Input::Key(Key::Left, Mods::NONE)));
        assert_eq!(app.view, View::Auto);
        key(&mut app, 'h');
        assert_eq!(app.view, View::Unified);
        key(&mut app, 'l');
        assert_eq!(app.view, View::Auto);
        let shown = screen(&mut app);
        let (y, row) = shown
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains("Layout ‹"))
            .expect("the layout row");
        let left = row.chars().position(|c| c == '‹').expect("‹");
        let right = row.chars().position(|c| c == '›').expect("›");
        let tap = |app: &mut App, x: usize| {
            app.event(Event::Input(Input::Click {
                x: u16::try_from(x).expect("column"),
                y: u16::try_from(y).expect("row"),
            }));
        };
        tap(&mut app, left);
        assert_eq!(app.view, View::Unified);
        tap(&mut app, right);
        assert_eq!(app.view, View::Auto);
    }

    /// In the panel `h`/`l` only step values: off a value row they do
    /// nothing. Highlighting and line numbers are on `H` and `L`, so they
    /// work from any row, the layout row `s` leaves the cursor on included.
    #[test]
    fn h_and_l_only_step_values_in_the_panel() {
        let mut app = app(140, 40);
        key(&mut app, ' ');
        let (highlight, numbers) = (app.highlight, app.line_numbers);
        key(&mut app, 'h');
        key(&mut app, 'l');
        assert_eq!(
            (app.highlight, app.line_numbers, app.view),
            (highlight, numbers, View::Auto)
        );
        assert!(!status(&mut app).contains("no command"));
        key(&mut app, 's');
        key(&mut app, 'H');
        key(&mut app, 'L');
        assert_eq!(app.view, View::Split);
        assert_eq!((app.highlight, app.line_numbers), (!highlight, !numbers));
        assert!(app.commands.is_some());
    }

    /// The theme row opens a list of every theme, grouped; the cursor
    /// tries each theme on the screen behind, `esc` brings back the one it
    /// opened on, `enter` keeps the one under the cursor and remembers it.
    #[test]
    fn the_theme_list_tries_themes_live_and_esc_restores() {
        let mut app = app(140, 40);
        // The built-ins only, whatever the machine's theme directory has.
        app.themes = theme::builtin_names()
            .map(|name| {
                let source = theme::dump(name).expect("built-in");
                (
                    name.to_owned(),
                    Theme::parse(name, &source, &mut Vec::new()),
                )
            })
            .collect();
        key(&mut app, ' ');
        let names = app
            .themes
            .iter()
            .map(|(n, t)| (n.clone(), t.group()))
            .collect();
        app.commands
            .as_mut()
            .expect("open")
            .open_themes(names, "default");
        // The list alone: what's around it is the panel's snapshot.
        let shown = screen(&mut app);
        let lines: Vec<Vec<char>> = shown.lines().map(|l| l.chars().collect()).collect();
        let top = lines
            .iter()
            .position(|l| l.iter().collect::<String>().contains("─ themes ─"))
            .expect("the list");
        let left = lines[top].iter().position(|&c| c == '┌').expect("corner");
        let right = left
            + lines[top][left..]
                .iter()
                .position(|&c| c == '┐')
                .expect("corner");
        let list: Vec<String> = lines[top..]
            .iter()
            .take_while(|l| l.get(left) != Some(&'└'))
            .map(|l| l[left..=right].iter().collect())
            .collect();
        insta::assert_snapshot!("theme_list", list.join("\n"));
        let down = |app: &mut App| app.event(Event::Input(Input::Key(Key::Down, Mods::NONE)));
        down(&mut app);
        down(&mut app);
        assert_eq!(app.theme_name, "github-dark");
        assert_eq!(
            app.theme.style(Role::Added).fg,
            Some(theme::Color::Rgb(0x3f, 0xb9, 0x50))
        );
        esc(&mut app);
        assert_eq!(app.theme_name, "default");
        assert_eq!(app.theme.style(Role::Added).fg, Some(theme::Color::Ansi(2)));
        assert!(
            app.commands.as_ref().is_some_and(|c| !c.themes_open()),
            "back to the panel"
        );
        let names = app
            .themes
            .iter()
            .map(|(n, t)| (n.clone(), t.group()))
            .collect();
        app.commands
            .as_mut()
            .expect("open")
            .open_themes(names, "default");
        for c in "latte".chars() {
            key(&mut app, c);
        }
        assert_eq!(app.theme_name, "catppuccin-latte");
        app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
        assert_eq!(app.theme_name, "catppuccin-latte");
        assert!(
            app.remembered
                .iter()
                .any(|(k, v)| *k == "theme" && v == "catppuccin-latte")
        );
        assert!(app.commands.as_ref().is_some_and(|c| !c.themes_open()));
        // The row's key steps forward and puts the cursor on it; there,
        // left steps back, without the list.
        key(&mut app, 'c');
        assert_eq!(app.theme_name, "gruvbox-light");
        app.event(Event::Input(Input::Key(Key::Left, Mods::NONE)));
        assert_eq!(app.theme_name, "catppuccin-latte");
        assert!(app.commands.as_ref().is_some_and(|c| !c.themes_open()));
    }

    /// `/` in the panel narrows it with the fuzzy matcher, marking what
    /// matched; `esc` clears the filter, then closes.
    #[test]
    fn the_panel_filters_fuzzily() {
        let mut app = app(140, 40);
        key(&mut app, ' ');
        for c in "/wrp".chars() {
            key(&mut app, c);
        }
        insta::assert_snapshot!("commands_filtered", screen(&mut app));
        app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
        esc(&mut app);
        assert!(app.commands.is_some(), "esc clears the filter first");
        esc(&mut app);
        assert!(app.commands.is_none());
    }

    #[test]
    fn a_tap_on_a_row_runs_it() {
        let mut app = app(140, 40);
        key(&mut app, ' ');
        let shown = screen(&mut app);
        let y = shown
            .lines()
            .position(|l| l.contains("Wrap long lines"))
            .expect("the wrap row");
        app.event(Event::Input(Input::Click {
            x: 70,
            y: u16::try_from(y).expect("row"),
        }));
        assert!(app.wrap);
    }

    /// The status hint follows focus: the diff's search and a way to the
    /// tree, or the tree's filter and a way back, then the menu. On a
    /// narrow screen `space` is `␣`, and the pane switch goes first when
    /// room runs short.
    #[test]
    fn the_status_bar_hint_follows_focus() {
        let tab = |app: &mut App| app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
        // The diff-focused frame is the `frame_wide` snapshot.
        let mut wide = app(120, 18);
        let shown = status(&mut wide);
        assert!(
            shown.ends_with("/ search · tab tree · space commands ─┘"),
            "{shown}"
        );
        tab(&mut wide);
        let shown = status(&mut wide);
        assert!(
            shown.ends_with("/ filter · tab diff · space commands ─┘"),
            "{shown}"
        );
        insta::assert_snapshot!("frame_tree_focused", screen(&mut wide));
        let mut phone = app(90, 20);
        let shown = status(&mut phone);
        assert!(
            shown.ends_with("/ search · tab tree · ␣ commands"),
            "{shown}"
        );
        tab(&mut phone);
        let shown = status(&mut phone);
        assert!(
            shown.ends_with("/ filter · tab diff · ␣ commands"),
            "{shown}"
        );
        let mut small = app(60, 20);
        let shown = status(&mut small);
        assert!(
            shown.ends_with("unified    / search · ␣ commands"),
            "{shown}"
        );
        let mut smaller = app(44, 20);
        let shown = status(&mut smaller);
        assert!(shown.ends_with("hunk        ␣ commands"), "{shown}");
    }

    /// The status bar names the keys in effect: a remapped leader and
    /// pane switch show, in both panes.
    #[test]
    fn the_status_bar_hint_follows_remapped_keys() {
        let mut config = Config::default();
        config.apply_source(
            "keybind = space=unbind\nkeybind = ctrl+k=leader\nkeybind = leader:z=toggle_wrap\nkeybind = tab=unbind\n",
            &mut Vec::new(),
        );
        let mut app = App::new(
            &config,
            Theme::default_theme(),
            "working tree".to_owned(),
            Some(diff("a.rs").change),
            (100, 10),
            Indicators::Classic,
        );
        let shown = status(&mut app);
        assert!(
            shown.ends_with("/ search · shift+tab tree · ctrl+k commands"),
            "{shown}"
        );
        app.event(Event::Input(Input::Key(Key::BackTab, Mods::NONE)));
        let shown = status(&mut app);
        assert!(
            shown.ends_with("/ filter · shift+tab diff · ctrl+k commands"),
            "{shown}"
        );
        app.event(Event::Input(Input::Key(Key::BackTab, Mods::NONE)));
        app.event(Event::Input(Input::Key(Key::Char('k'), Mods::CTRL)));
        assert!(app.commands.is_some());
        key(&mut app, 'z');
        assert!(app.wrap, "the remapped entry ran");
    }

    /// A modified file whose new side is `text`.
    fn searchable(path: &str, text: &str) -> FileDiff {
        let old = Text::new(b"one\ntwo\n".to_vec());
        let new = Text::new(text.as_bytes().to_vec());
        let hunks = hunks(&old, &new, &Options::default());
        let side = Some(Side::Worktree {
            mode: Mode::File,
            id: None,
        });
        FileDiff {
            change: FileChange {
                path: path.into(),
                status: Status::Modified,
                old: side,
                new: side,
                staging: None,
            },
            old,
            new,
            binary: false,
            too_large: false,
            note: None,
            hunks,
        }
    }

    /// Two files to search: `src/a.rs` with `needle` twice, `src/b.rs`
    /// with `Needle` once, both diffed.
    fn search_app() -> App {
        let a = searchable("src/a.rs", "one\nneedle here\nand a needle\n");
        let b = searchable("src/b.rs", "Needle\ntwo\n");
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "working tree".to_owned(),
            Some(a.change.clone()),
            (100, 20),
            Indicators::Classic,
        );
        app.event(Event::Enumerated(b.change.clone()));
        app.event(Event::EnumerationDone(None));
        for (i, fd) in [a, b].into_iter().enumerate() {
            let version = app.version(i);
            app.event(Event::Diffed(Done::Diff {
                file: i,
                version,
                result: Ok(Box::new(Loaded::new(fd, Whitespace::Exact, None))),
            }));
        }
        app
    }

    /// The search's jobs, done as the workers would: the matches in the
    /// file shown, and the counts of the others.
    fn work(app: &mut App) {
        for job in app.jobs() {
            let done = match job {
                Job::Find {
                    file,
                    version,
                    diff,
                    hidden,
                    needle,
                    id,
                    ..
                } => Done::Found {
                    file,
                    version,
                    id,
                    matches: view::find(&diff, Some(&hidden), &needle, || false),
                    diff,
                    hidden,
                },
                Job::Search {
                    file,
                    version,
                    needle,
                    id,
                    ..
                } => Done::Search {
                    file,
                    version,
                    id,
                    count: Some(needle.find_all(b"Needle\n").len()),
                },
                _ => continue,
            };
            app.event(Event::Diffed(done));
        }
    }

    /// The status bar: the bottom row.
    fn status(app: &mut App) -> String {
        screen(app).lines().last().unwrap_or_default().to_owned()
    }

    /// `/` searches as it's typed, the workers marking matches on screen
    /// as they find them; `enter` goes to the first; `n`/`N` go between
    /// matches and on into the next file; the status bar counts; `esc`
    /// gives `n` back to changes.
    #[test]
    fn the_diff_search_marks_jumps_and_counts() {
        let mut app = search_app();
        app.draw();
        for c in "/needle".chars() {
            key(&mut app, c);
        }
        assert!(app.doc.search.is_none(), "typing waits for no scan");
        assert!(screen(&mut app).contains("… in this file"));
        work(&mut app);
        let marks = app.doc.search.as_ref().map(|m| m.matches.list.len());
        assert_eq!(marks, Some(2), "marked once found");
        app.draw();
        let lit = app.theme.style(Role::SearchMatch);
        let cells = (0..20)
            .flat_map(|y| app.screen.shown_grid().row(y).iter())
            .filter(|c| c.style() == lit)
            .count();
        assert_eq!(
            cells,
            2 * "needle".len(),
            "both matches drawn in the match style"
        );
        let shown = status(&mut app);
        assert!(shown.contains("/needle▏  2 in this file"), "{shown}");
        app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
        work(&mut app);
        let shown = status(&mut app);
        assert!(shown.contains("match 1/3"), "{shown}");
        key(&mut app, 'n');
        let shown = status(&mut app);
        assert!(shown.contains("match 2/3"), "{shown}");
        // On into src/b.rs, where smart case still finds `Needle`, once
        // its matches arrive.
        key(&mut app, 'n');
        assert_eq!(app.doc.pos.file, 1);
        work(&mut app);
        let shown = status(&mut app);
        assert!(shown.contains("match 3/3"), "{shown}");
        key(&mut app, 'N');
        assert_eq!(app.doc.pos.file, 0);
        esc(&mut app);
        assert!(app.search.is_none());
        key(&mut app, 'g');
        key(&mut app, 'n');
        assert!(app.search.is_none(), "n goes to changes again");
    }

    /// A scan for a cleared search that reports after the next search
    /// started is dropped: search ids never repeat.
    #[test]
    fn a_late_count_from_a_cleared_search_isnt_taken_for_the_next_one() {
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "working tree".to_owned(),
            Some(searchable("a.rs", "f\n").change),
            (100, 20),
            Indicators::Classic,
        );
        app.event(Event::EnumerationDone(None));
        typed(&mut app, "/f");
        let old = app.search.as_ref().map(|s| s.id).expect("searching");
        app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
        typed(&mut app, "/g");
        app.event(Event::Diffed(Done::Search {
            file: 0,
            version: app.version(0),
            id: old,
            count: Some(5),
        }));
        let counts = app.search.as_ref().map(|s| s.counts.clone());
        assert_eq!(counts, Some(vec![None]));
    }

    /// A search typed while the list is still arriving survives the sort
    /// at the end of it: an untracked file listed last moves ahead of the
    /// file shown, and the matches stay on the file they were found in.
    #[test]
    fn search_marks_follow_their_file_when_the_list_is_sorted() {
        let tracked = searchable("src/z.rs", "needle\nneedle\n");
        let untracked = FileDiff {
            change: FileChange {
                status: Status::Untracked,
                old: None,
                ..searchable("src/a.rs", "").change
            },
            ..searchable("src/a.rs", "a needle\n")
        };
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "working tree".to_owned(),
            Some(tracked.change.clone()),
            (100, 20),
            Indicators::Classic,
        );
        app.event(Event::Enumerated(untracked.change.clone()));
        for (file, fd) in [tracked, untracked].into_iter().enumerate() {
            let version = app.version(file);
            app.event(Event::Diffed(Done::Diff {
                file,
                version,
                result: Ok(Box::new(Loaded::new(fd, Whitespace::Exact, None))),
            }));
        }
        app.draw();
        typed(&mut app, "/needle");
        let stale_jobs = app.jobs();
        work(&mut app);
        assert_eq!(app.doc.search.as_ref().map(|m| m.file), Some(0));

        app.event(Event::EnumerationDone(None));
        assert_eq!(app.doc.files[0].change.path, "src/a.rs");
        assert_eq!(app.doc.pos.file, 1, "still on src/z.rs");
        let marked = app
            .doc
            .search
            .as_ref()
            .map(|m| &app.doc.files[m.file].change.path);
        assert!(marked.is_none_or(|path| path == "src/z.rs"), "{marked:?}");
        // A scan started before the sort reports by the old positions.
        for job in stale_jobs {
            if let Job::Find {
                file,
                version,
                diff,
                hidden,
                needle,
                id,
                ..
            } = job
            {
                app.event(Event::Diffed(Done::Found {
                    file,
                    version,
                    id,
                    matches: view::find(&diff, Some(&hidden), &needle, || false),
                    diff,
                    hidden,
                }));
            }
        }
        work(&mut app);
        let marks = app
            .doc
            .search
            .as_ref()
            .map(|m| (m.file, m.matches.list.len()));
        assert_eq!(marks, Some((1, 2)), "src/z.rs's two matches");
        app.draw();
        let lit = app.theme.style(Role::SearchMatch);
        let cells = (0..20)
            .flat_map(|y| app.screen.shown_grid().row(y).iter())
            .filter(|c| c.style() == lit)
            .count();
        assert_eq!(cells, 2 * "needle".len());
        let counts = app.search.as_ref().map(|s| s.counts.clone());
        assert_eq!(counts.and_then(|c| c.first().copied()), Some(Some(1)));
    }

    /// A file whose diff failed has nothing to scan: `enter` and `n` on it
    /// go on to the files that have matches instead of waiting.
    #[test]
    fn a_search_on_a_failed_file_goes_on_to_the_others() {
        let mut app = search_app();
        app.event(Event::Enumerated(diff("src/c.rs").change));
        app.event(Event::Diffed(Done::Diff {
            file: 2,
            version: app.version(2),
            result: Err("unreadable".to_owned()),
        }));
        key(&mut app, 'f');
        key(&mut app, 'f');
        assert_eq!(app.doc.pos.file, 2);
        typed(&mut app, "/needle");
        app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
        work(&mut app);
        assert!(!status(&mut app).contains("searching this file"));
        key(&mut app, 'N');
        work(&mut app);
        assert_eq!(app.doc.pos.file, 1, "back to src/b.rs and its match");
        let current = app.doc.search.as_ref().and_then(|m| m.current);
        assert_eq!(current, Some(0), "{}", status(&mut app));
    }

    /// A pattern typed over a running scan stops it: the scan asks, and
    /// the search's id has moved on.
    #[test]
    fn a_newer_pattern_stops_the_scan_of_an_older_one() {
        let mut app = search_app();
        typed(&mut app, "/ne");
        let jobs = app.jobs();
        let Some(Job::Find {
            diff,
            hidden,
            live,
            id,
            ..
        }) = jobs.into_iter().find(|j| matches!(j, Job::Find { .. }))
        else {
            panic!("a scan of the file shown");
        };
        key(&mut app, 'e');
        let stale = || live.load(Ordering::Relaxed) != id;
        assert!(stale());
        let needle = Needle::new("ne");
        assert_eq!(view::find(&diff, Some(&hidden), &needle, stale), None);
        assert!(view::find(&diff, Some(&hidden), &needle, || false).is_some());
    }

    /// A capital letter makes the search match case.
    #[test]
    fn smart_case() {
        assert_eq!(Needle::new("needle").find_all(b"Needle needle").len(), 2);
        assert_eq!(Needle::new("Needle").find_all(b"Needle needle").len(), 1);
    }

    /// `/` in the tree filters it fuzzily: matching files with their
    /// folders, the matched letters marked; `f` keeps to them; `esc`
    /// clears it.
    #[test]
    fn the_tree_filter_scopes_moves_and_clears() {
        let mut app = files_app(120, 12);
        app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
        for c in "/fair\r".chars() {
            app.event(Event::Input(Input::Key(
                if c == '\r' { Key::Enter } else { Key::Char(c) },
                Mods::NONE,
            )));
        }
        let shown = screen(&mut app);
        assert!(shown.contains("Files · 1/4 · fair"), "{shown}");
        assert!(shown.contains("▼ kernel/sched/"), "{shown}");
        assert!(!shown.contains(".clang-format"), "{shown}");
        assert!(
            shown.lines().next().is_some_and(|l| l.contains("fair.c")),
            "{shown}"
        );
        app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
        let at = app.doc.pos.file;
        key(&mut app, 'f');
        assert_eq!(app.doc.pos.file, at, "no other file passes the filter");
        app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
        esc(&mut app);
        let shown = screen(&mut app);
        assert!(shown.contains(".clang-format"), "{shown}");
        assert!(shown.contains("tree filter cleared"), "{shown}");
    }

    /// `esc` backs out a layer at a time: the overlay, the search, the
    /// selection, the tree's filter, the tree's focus; then it only says
    /// how to quit.
    #[test]
    fn esc_backs_out_one_layer_at_a_time() {
        let mut app = app(100, 20);
        app.draw();
        let esc = |app: &mut App| app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
        typed(&mut app, "/line");
        app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
        key(&mut app, 'v');
        key(&mut app, '?');
        assert_eq!(esc(&mut app), None);
        assert!(app.help.is_none(), "the overlay first");
        assert!(app.search.is_some() && app.doc.selection.is_some());
        esc(&mut app);
        assert!(app.search.is_none(), "then the search");
        assert!(app.doc.selection.is_some());
        esc(&mut app);
        assert!(app.doc.selection.is_none(), "then the selection");
        app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
        typed(&mut app, "/zz");
        app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
        assert_eq!(app.tree.filter(), "zz");
        esc(&mut app);
        assert_eq!(app.tree.filter(), "", "then the tree's filter");
        assert_eq!(app.pane, Pane::Tree);
        esc(&mut app);
        assert_eq!(app.pane, Pane::Diff, "then the tree's focus");
        assert_eq!(esc(&mut app), None, "never quits");
        let status = status(&mut app);
        assert!(status.contains("press q to quit"), "{status}");
    }

    /// `keybind = esc=quit`: `esc` still backs out of the search, the
    /// selection and the tree first, and quits only with nothing left.
    #[test]
    fn esc_bound_to_quit_backs_out_of_layers_first() {
        let mut config = Config::default();
        config.apply_source("keybind = esc=quit\n", &mut Vec::new());
        let mut app = App::new(
            &config,
            Theme::default_theme(),
            "uncommitted".to_owned(),
            Some(diff("a.rs").change),
            (120, 20),
            Indicators::Classic,
        );
        app.draw();
        let esc = |app: &mut App| app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
        typed(&mut app, "/line");
        app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
        key(&mut app, '?');
        assert_eq!(esc(&mut app), None);
        assert!(app.help.is_none(), "the overlay first");
        assert_eq!(esc(&mut app), None);
        assert!(app.search.is_none(), "then the search");
        app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
        assert_eq!(esc(&mut app), None);
        assert_eq!(app.pane, Pane::Diff, "then the tree's focus");
        assert_eq!(esc(&mut app), Some(Action::Quit), "then it quits");
    }

    /// The panel's copy copies the selection made before it opened.
    #[test]
    fn the_panels_copy_copies_the_selection() {
        let mut app = app(140, 40);
        app.draw();
        for c in "vj ".chars() {
            key(&mut app, c);
        }
        assert!(app.commands.is_some());
        let copied = app.event(Event::Input(Input::Key(Key::Char('y'), Mods::NONE)));
        assert_eq!(
            copied,
            Some(Action::Copy {
                text: b"line 1\nline 2".to_vec(),
                note: "copied 2 lines".to_owned(),
            })
        );
    }

    /// Highlighting or whitespace toggled from the panel diffs every file
    /// again with the new settings.
    #[test]
    fn toggling_highlighting_diffs_again() {
        let mut app = app(140, 40);
        app.draw();
        let version = app.version(0);
        key(&mut app, ' ');
        let done = app.event(Event::Input(Input::Key(Key::Char('H'), Mods::NONE)));
        assert_eq!(
            done,
            Some(Action::Rediff {
                highlight: false,
                whitespace: Whitespace::Exact
            })
        );
        assert_ne!(app.version(0), version, "work asked for before is void");
        assert!(
            app.doc.files[0].wants_diff(),
            "the shown file is diffed again"
        );
        let done = app.event(Event::Input(Input::Key(Key::Char('W'), Mods::NONE)));
        assert_eq!(
            done,
            Some(Action::Rediff {
                highlight: false,
                whitespace: Whitespace::IgnoreAll
            })
        );
    }

    /// `X` switches every file, not just the one shown, and is remembered
    /// as `context`: `full`, or the number of lines it goes back to. The
    /// whitespace toggle sets both whitespace keys; `w` remembers watching.
    #[test]
    fn whole_files_whitespace_and_watch_are_remembered() {
        let mut app = app(120, 18);
        app.set_context_lines(5);
        key(&mut app, 'X');
        assert!(app.doc.files.iter().all(|e| app.doc.is_full(e)));
        assert_eq!(app.take_remembered(true), [("context", "full".to_owned())]);
        key(&mut app, 'X');
        assert_eq!(app.take_remembered(true), [("context", "5".to_owned())]);
        key(&mut app, ' ');
        key(&mut app, 'W');
        esc(&mut app);
        key(&mut app, 'w');
        assert_eq!(
            app.take_remembered(true),
            [
                ("ignore-all-space", "true".to_owned()),
                ("ignore-space-change", "false".to_owned()),
                ("watch", "true".to_owned())
            ]
        );
        // A watch that couldn't start isn't remembered.
        key(&mut app, 'w');
        key(&mut app, 'w');
        app.set_watching(false);
        assert_eq!(app.take_remembered(true), []);
        app.set_piped();
        key(&mut app, 'X');
        assert_eq!(app.take_remembered(true), []);
    }

    /// `tree = hide` starts without the tree even on a wide screen; `t`
    /// shows it, and that choice is remembered.
    #[test]
    fn tree_hide_starts_hidden_and_t_shows_it() {
        let config = Config {
            tree: TreeMode::Hide,
            ..Config::default()
        };
        let mut app = App::new(
            &config,
            Theme::default_theme(),
            "working tree".to_owned(),
            Some(diff("a.rs").change),
            (120, 10),
            Indicators::Classic,
        );
        assert!(!screen(&mut app).contains("┌ Files"));
        key(&mut app, 't');
        assert!(screen(&mut app).contains("┌ Files"));
        assert_eq!(app.take_remembered(true), [("tree", "show".to_owned())]);
    }

    /// `tree = show` opens the tree on a narrow screen too, over the diff
    /// and with the keys; `auto`, or a `show` remembered from a wide
    /// screen, starts without it there.
    #[test]
    fn tree_show_opens_it_over_a_narrow_screen_unless_remembered() {
        let start = |tree, remembered| {
            let config = Config {
                tree,
                ..Config::default()
            };
            let mut app = App::new(
                &config,
                Theme::default_theme(),
                "working tree".to_owned(),
                Some(diff("a.rs").change),
                (50, 10),
                Indicators::Classic,
            );
            app.start_tree(remembered);
            (app.tree_shown, app.pane)
        };
        assert_eq!(start(TreeMode::Show, false), (true, Pane::Tree));
        assert_eq!(start(TreeMode::Show, true), (false, Pane::Diff));
        assert_eq!(start(TreeMode::Auto, false), (false, Pane::Diff));
    }

    /// Without files, a tree over the diff still says why the list is
    /// empty.
    #[test]
    fn an_empty_tree_over_the_diff_shows_the_empty_note() {
        let config = Config {
            tree: TreeMode::Show,
            ..Config::default()
        };
        let mut app = App::new(
            &config,
            Theme::default_theme(),
            "working tree".to_owned(),
            None,
            (50, 12),
            Indicators::Classic,
        );
        app.start_tree(false);
        assert!(screen(&mut app).contains("listing changes…"));
        app.event(Event::EnumerationDone(None));
        let text = screen(&mut app);
        assert!(
            text.contains("┌ Files") && text.contains("no changes"),
            "{text}"
        );
    }

    /// A remembered split on a screen too narrow for it renders unified,
    /// and nothing is written over the choice; only `s` changes it.
    #[test]
    fn a_remembered_split_renders_unified_when_narrow_and_stays_remembered() {
        let config = Config {
            view: View::Split,
            ..Config::default()
        };
        let first = diff("a.rs");
        let mut app = App::new(
            &config,
            Theme::default_theme(),
            "working tree".to_owned(),
            Some(first.change.clone()),
            (30, 10),
            Indicators::Classic,
        );
        app.event(Event::EnumerationDone(None));
        app.event(Event::Diffed(Done::Diff {
            file: 0,
            version: app.version(0),
            result: Ok(Box::new(Loaded::new(first, Whitespace::Exact, None))),
        }));
        // Split, a row shows its line on both halves; unified, once.
        let halves = |app: &mut App| {
            let shown = screen(app);
            let row = shown
                .lines()
                .find(|l| l.contains("line 1"))
                .unwrap_or_default()
                .to_owned();
            row.matches("line 1").count()
        };
        assert_eq!(halves(&mut app), 1, "too narrow for halves");
        app.resize(160, 10);
        assert_eq!(halves(&mut app), 2);
        app.resize(30, 10);
        assert_eq!(halves(&mut app), 1);
        assert_eq!(app.view, View::Split);
        assert_eq!(app.take_remembered(true), []);
        key(&mut app, 's');
        assert_eq!(app.take_remembered(true), [("view", "unified".to_owned())]);
    }

    /// Line numbers and hunk numbers off: the change marks and the
    /// functions stay.
    #[test]
    fn line_numbers_and_hunk_headers_can_be_hidden() {
        let mut app = app(60, 12);
        for c in [' ', 'L', 'm'] {
            key(&mut app, c);
        }
        esc(&mut app);
        let shown = screen(&mut app);
        insta::assert_snapshot!(shown);
        assert_eq!(
            app.take_remembered(true),
            [
                ("line-numbers", "false".to_owned()),
                ("hunk-headers", "false".to_owned())
            ]
        );
    }

    fn copy(app: &mut App) -> Option<Action> {
        app.event(Event::Input(Input::Key(Key::Char('y'), Mods::NONE)))
    }

    /// `v` `j` `y` copies the code of the lines selected, nothing else; a
    /// drag selects too; `Y` copies the path.
    #[test]
    fn v_j_y_and_a_drag_copy_lines_and_shift_y_the_path() {
        let mut app = app(100, 20);
        app.draw();
        for c in "vjj".chars() {
            key(&mut app, c);
        }
        let shown = status(&mut app);
        assert!(shown.contains("3 new lines · y copies"), "{shown}");
        assert_eq!(
            copy(&mut app),
            Some(Action::Copy {
                text: b"line 1\nline 2\nline 3".to_vec(),
                note: "copied 3 lines".to_owned(),
            })
        );
        assert_eq!(copy(&mut app), None, "the copy ends the selection");
        // Pressed in the code of `line 4` (row 5, under the frame's top and
        // the hunk's `@@` row) and dragged two rows down: by characters, on
        // the old side, since the press was on a removed line; copied as
        // the button is let go, and again by `y`.
        app.draw();
        app.event(Event::Input(Input::Click { x: 40, y: 5 }));
        app.event(Event::Input(Input::Drag { x: 40, y: 7 }));
        let chars = Some(Action::Copy {
            text: b"ne 4\nlin".to_vec(),
            note: "copied 2 lines".to_owned(),
        });
        assert_eq!(
            app.event(Event::Input(Input::Release { x: 40, y: 7 })),
            chars
        );
        assert_eq!(copy(&mut app), chars);
        // Dragged into the new side's half, it keeps to the old side: to
        // the end of the line there.
        app.draw();
        app.clicks = None;
        app.event(Event::Input(Input::Click { x: 40, y: 5 }));
        app.event(Event::Input(Input::Drag { x: 90, y: 5 }));
        assert_eq!(
            app.event(Event::Input(Input::Release { x: 90, y: 5 })),
            Some(Action::Copy {
                text: b"ne 4".to_vec(),
                note: "copied 4 chars".to_owned(),
            })
        );
        // A double click selects the word, a triple click the line.
        for (clicks, text) in [(2, &b"line"[..]), (3, b"line 4")] {
            app.draw();
            app.clicks = None;
            let mut copied = None;
            for _ in 0..clicks {
                app.event(Event::Input(Input::Click { x: 40, y: 5 }));
                copied = app.event(Event::Input(Input::Release { x: 40, y: 5 }));
            }
            let Some(Action::Copy { text: got, .. }) = copied else {
                panic!("{clicks} clicks copy: {copied:?}");
            };
            assert_eq!(got, text);
            app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
        }
        app.clicks = None;
        // The same drag over the line numbers selects whole lines.
        app.draw();
        app.event(Event::Input(Input::Click { x: 30, y: 5 }));
        app.event(Event::Input(Input::Drag { x: 30, y: 7 }));
        assert_eq!(
            app.event(Event::Input(Input::Release { x: 30, y: 7 })),
            Some(Action::Copy {
                text: b"line 4\nline 5".to_vec(),
                note: "copied 2 lines".to_owned(),
            })
        );
        assert_eq!(
            app.event(Event::Input(Input::Key(Key::Char('Y'), Mods::NONE))),
            Some(Action::Copy {
                text: b"src/some/deeply/nested/module/file.rs".to_vec(),
                note: "copied src/some/deeply/nested/module/file.rs".to_owned(),
            })
        );
    }

    /// `n` into a file not diffed yet waits at its top; `n` again waits
    /// too, rather than skipping the file's changes; the diff arriving
    /// opens the file as it opens the first time, its first change in view
    /// (on the first screen here, so from the top), and the next `n` goes
    /// on past that change.
    #[test]
    fn n_into_a_file_not_diffed_yet_waits_for_its_diff() {
        let mut app = app(100, 16);
        for _ in 0..3 {
            key(&mut app, 'n');
        }
        assert_eq!((app.doc.pos.file, app.doc.pos.part), (1, 0));
        key(&mut app, 'n');
        assert_eq!((app.doc.pos.file, app.doc.pos.part), (1, 0));
        assert!(screen(&mut app).contains("waiting for this file's diff"));
        let loaded = Loaded::new(diff("src/zz.rs"), Whitespace::Exact, None);
        app.event(Event::Diffed(Done::Diff {
            file: 1,
            version: app.version(1),
            result: Ok(Box::new(loaded)),
        }));
        assert_eq!(app.doc.pos.file, 1);
        assert!(app.doc.pos.part > 0, "{:?}", app.doc.pos);
        let shown = screen(&mut app);
        let top = shown.lines().nth(1).unwrap_or_default();
        assert!(top.contains("@@ -1,7"), "{shown}");
        assert!(shown.contains("-│ line 4"), "{shown}");
        // The second change is near the end: the view goes as far down as
        // the file goes.
        key(&mut app, 'n');
        let shown = screen(&mut app);
        let top = shown.lines().nth(1).unwrap_or_default();
        assert!(top.contains("+│ line four"), "{shown}");
        assert!(shown.contains("-│ line 50"), "{shown}");
    }

    /// With `context = full`, `f` opens a file whose first change is at
    /// line 400 a few rows above that change, whether its diff came before
    /// or after; a tap's release meanwhile changes nothing.
    #[test]
    fn a_full_file_opens_at_its_first_change() {
        let late = || {
            let old = (1..=500)
                .map(|i| format!("line {i}\n"))
                .collect::<Vec<_>>()
                .concat();
            let new = old.replace("line 400\n", "line four hundred\n");
            FileDiff {
                hunks: hunks(
                    &Text::new(old.clone().into()),
                    &Text::new(new.clone().into()),
                    &Options::default(),
                ),
                old: Text::new(old.into()),
                new: Text::new(new.into()),
                ..diff("src/zz.rs")
            }
        };
        let mut config = Config::default();
        config.apply_source("context = full\n", &mut Vec::new());
        for diff_first in [true, false] {
            let mut app = app_of(&config, "working tree", diff("src/a.rs"), (100, 16));
            let deliver = |app: &mut App| {
                app.event(Event::Diffed(Done::Diff {
                    file: 1,
                    version: app.version(1),
                    result: Ok(Box::new(Loaded::new(late(), Whitespace::Exact, None))),
                }));
            };
            if diff_first {
                deliver(&mut app);
                key(&mut app, 'f');
            } else {
                key(&mut app, 'f');
                app.event(Event::Input(Input::Release { x: 50, y: 5 }));
                deliver(&mut app);
            }
            let shown = screen(&mut app);
            let first = shown.lines().nth(1).unwrap_or_default();
            assert!(first.contains("397 397"), "{shown}");
            assert!(shown.contains("-│ line 400"), "{shown}");
        }
    }

    /// `keybind` lines in the config change what keys do in the viewer,
    /// and the help lists the keys as they are.
    #[test]
    fn remapped_keys_act_and_the_help_shows_them() {
        let mut config = Config::default();
        let mut warnings = Vec::new();
        config.apply_source(
            "keybind = n=next_file\nkeybind = q=unbind\nkeybind = ctrl+q=quit\nkeybind = ?=unbind\nkeybind = F1=help\n",
            &mut warnings,
        );
        assert_eq!(warnings.len(), 1, "F1 isn't a key name: {warnings:?}");
        config.apply_source("keybind = f1=help\n", &mut warnings);
        let first = diff("a.rs");
        let mut app = App::new(
            &config,
            Theme::default_theme(),
            "working tree".to_owned(),
            Some(first.change),
            (100, 46),
            Indicators::Classic,
        );
        app.event(Event::Enumerated(diff("b.rs").change));
        app.event(Event::EnumerationDone(None));
        key(&mut app, 'n');
        assert_eq!(app.doc.pos.file, 1, "n moves to the next file now");
        assert_eq!(
            app.event(Event::Input(Input::Key(Key::Char('q'), Mods::NONE))),
            None
        );
        app.event(Event::Input(Input::Key(Key::F(1), Mods::NONE)));
        let has = |app: &mut App, row: &str| {
            let shown = screen(app);
            let found = shown.lines().any(|l| {
                l.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .contains(row)
            });
            assert!(found, "{row}:\n{shown}");
        };
        has(&mut app, "} next change");
        typed(&mut app, "2");
        has(&mut app, "│ f n next file");
        typed(&mut app, "/quit");
        has(&mut app, "│ ctrl+c ctrl+q quit Other");
        app.event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
        typed(&mut app, "/help");
        has(&mut app, "│ f1 show this help Other");
        app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
        app.event(Event::Input(Input::Key(Key::F(1), Mods::NONE)));
        assert!(!screen(&mut app).contains("this help"));
        assert_eq!(
            app.event(Event::Input(Input::Key(Key::Char('q'), Mods::CTRL))),
            Some(Action::Quit)
        );
    }

    /// A tree of the given files, all counted, `width` columns wide: the
    /// panel's columns of the screen.
    fn tree_panel(width: usize, files: &[(&str, usize, usize)]) -> String {
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "range".to_owned(),
            None,
            (width, 11),
            Indicators::Classic,
        );
        for (path, _, _) in files {
            app.event(Event::Enumerated(diff(path).change));
        }
        app.event(Event::EnumerationDone(None));
        let counts = files
            .iter()
            .enumerate()
            .map(|(i, &(_, added, removed))| (i, app.version(i), view::Counts { added, removed }))
            .collect();
        app.event(Event::Diffed(Done::Counts { counts }));
        if width < SIDE_TREE_WIDTH {
            app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
        }
        let columns = app.tree_columns().expect("the tree shows");
        // Between the title row and the status row.
        screen(&mut app)
            .lines()
            .skip(1)
            .take(9)
            .map(|l| {
                l.chars()
                    .take(columns)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    const FILES: &[(&str, usize, usize)] = &[
        (".clang-format", 12, 3),
        ("Documentation/admin-guide/kernel-parameters.txt", 1234, 56),
        (
            "drivers/net/wireless/realtek/rtw89/rtw8852c_table.c",
            54321,
            12345,
        ),
        ("kernel/sched/fair.c", 98765, 43210),
    ];

    /// The tree takes just over a quarter of a wide screen (all of a
    /// narrow one, over the diff): short names show whole, and long names
    /// are cut before the counts column, whose counts stay whole.
    #[test]
    fn the_tree_widens_with_the_screen_and_keeps_counts_whole() {
        insta::assert_snapshot!(tree_panel(80, FILES), @r"
            ┌ Files · 4 +154332 -55614 ────────────────────────────────────────────────────┐
            │ M .clang-format                                                   +12     -3 │
            │ ▼ Documentation/admin-guide/                                                 │
            │   M kernel-parameters.txt                                       +1234    -56 │
            │ ▼ drivers/net/wireless/realtek/rtw89/                                        │
            │   M rtw8852c_table.c                                           +54321 -12345 │
            │ ▼ kernel/sched/                                                              │
            │   M fair.c                                                     +98765 -43210 │
            └«─────────────────────────────────────────────────────────────────────────────┘
        ");
        insta::assert_snapshot!(tree_panel(120, FILES), @r"
        ┌ Files · 4 +154332 -55614 ─────┐
        │ M .clang-format    +12     -3 │
        │ ▼ Documentation/admin-guide/  │
        │   M …meters.txt  +1234    -56 │
        │ ▼ …et/wireless/realtek/rtw89/ │
        │   M …2c_table.c +54321 -12345 │
        │ ▼ kernel/sched/               │
        │   M fair.c      +98765 -43210 │
        └«──────────────────────────────┘
        ");
        insta::assert_snapshot!(tree_panel(200, FILES), @r"
        ┌ Files · 4 +154332 -55614 ────────────────────────────┐
        │ M .clang-format                           +12     -3 │
        │ ▼ Documentation/admin-guide/                         │
        │   M kernel-parameters.txt               +1234    -56 │
        │ ▼ drivers/net/wireless/realtek/rtw89/                │
        │   M rtw8852c_table.c                   +54321 -12345 │
        │ ▼ kernel/sched/                                      │
        │   M fair.c                             +98765 -43210 │
        └«─────────────────────────────────────────────────────┘
        ");
    }

    /// Additions only, short and long names at several depths: the
    /// counts stand in one column, the long names cut before it.
    #[test]
    fn tree_counts_stand_in_one_column() {
        let files = [
            ("README.md", 3, 0),
            (
                "crates/dffy/src/tui/snapshots/dffy__tui__app__tests__a_very_long_name.snap",
                127,
                0,
            ),
            ("crates/dffy/src/tui/app.rs", 12, 0),
            ("x", 1, 0),
        ];
        insta::assert_snapshot!(tree_panel(120, &files));
    }

    /// A viewer of a piped diff, read whole.
    fn piped_app(input: &[u8], width: usize, height: usize) -> App {
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "stdin".to_owned(),
            None,
            (width, height),
            Indicators::Classic,
        );
        app.set_piped();
        for item in piped::items(input) {
            app.event(Event::Piped(item));
        }
        app.event(Event::EnumerationDone(None));
        app
    }

    /// Does the workers' diffing of piped files, on this thread.
    fn read_back(app: &mut App) {
        app.draw();
        for job in app.jobs() {
            if let Job::Diff {
                file,
                version,
                change,
                piped: Some(piped),
            } = job
            {
                let (fd, headings) = piped.diff(change).expect("in memory");
                app.event(Event::Diffed(Done::Diff {
                    file,
                    version,
                    result: Ok(Box::new(Loaded::piped(fd, headings))),
                }));
            }
        }
    }

    const LOG: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/git-log-color.diff"
    ));

    /// `git log -p` in the viewer: each commit a top-level node of the tree
    /// with its files beneath, its header and file list as its view, and
    /// `n` going on from one commit's changes into the next's.
    #[test]
    fn a_piped_log_shows_commits_in_the_tree_and_their_headers() {
        let mut app = piped_app(LOG, 120, 24);
        read_back(&mut app);
        insta::assert_snapshot!("piped_log_commit", screen(&mut app));
        key(&mut app, 'n');
        read_back(&mut app);
        insta::assert_snapshot!("piped_log_file", screen(&mut app));
        key(&mut app, 'n');
        read_back(&mut app);
        let shown = status(&mut app);
        assert!(
            shown.contains(" commit 2/2 ") && shown.contains(" 1/7 · "),
            "{shown}"
        );
        assert!(
            shown.contains("→ commit 2/2: 80f116f Widen calc"),
            "{shown}"
        );
        // The unchanged lines between hunks aren't in the input.
        key(&mut app, 'x');
        assert!(
            screen(&mut app).contains("a piped diff has only its hunks"),
            "{}",
            screen(&mut app)
        );
        // Collapsing the first commit hides its file in the tree.
        app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
        key(&mut app, 'g');
        key(&mut app, 'h');
        let shown = screen(&mut app);
        let rows: Vec<&str> = shown.lines().collect();
        assert!(rows[1].contains("▶ 1fcf726 Add a third line"), "{shown}");
        assert!(rows[2].contains("▼ 80f116f Widen calc"), "{shown}");
    }

    /// `dffy show`: the commit's files are counted in the background, and
    /// its header gives their total only once every one is counted, never
    /// the zeros or partial sum it has before.
    #[test]
    fn a_commit_header_waits_for_every_files_counts() {
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "show HEAD".to_owned(),
            None,
            (100, 20),
            Indicators::Classic,
        );
        app.event(Event::Piped(piped::Item::Commit(Arc::new(
            dffy_core::patch::Commit {
                id: Some("0123abc".repeat(5) + "01234"),
                subject: "Change two files".to_owned(),
                lines: vec![(
                    dffy_core::patch::TextKind::CommitMessage,
                    "    Change two files".into(),
                )],
            },
        ))));
        for path in ["a.rs", "b.rs"] {
            app.event(Event::Enumerated(diff(path).change));
        }
        app.event(Event::EnumerationDone(None));
        let shows = |app: &mut App, summary: &str| {
            let shown = screen(app);
            assert!(shown.contains(summary), "{shown}");
        };
        shows(&mut app, "2 files …");
        let count = |added, removed| view::Counts { added, removed };
        app.event(Event::Diffed(Done::Counts {
            counts: vec![(1, app.version(1), count(3, 1))],
        }));
        shows(&mut app, "2 files …");
        app.event(Event::Diffed(Done::Counts {
            counts: vec![(2, app.version(2), count(2, 0))],
        }));
        shows(&mut app, "2 files +5 -1");
    }

    /// `u` from the diff selects the folder of the file shown; each `u`
    /// after climbs one more.
    #[test]
    fn u_climbs_the_tree_a_folder_at_a_time() {
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "range".to_owned(),
            None,
            (120, 20),
            Indicators::Classic,
        );
        for path in [
            "README.md",
            "src/app/main.rs",
            "src/app/view.rs",
            "src/lib.rs",
        ] {
            app.event(Event::Enumerated(diff(path).change));
        }
        app.event(Event::EnumerationDone(None));
        key(&mut app, 'f');
        key(&mut app, 'u');
        key(&mut app, 'h');
        assert!(screen(&mut app).contains("▶ app/"), "{}", screen(&mut app));
        key(&mut app, 'l');
        key(&mut app, 'u');
        key(&mut app, 'u');
        key(&mut app, 'h');
        let shown = screen(&mut app);
        assert!(shown.contains("▶ src/"), "{shown}");
        assert!(!shown.contains("▼ app/"), "{shown}");
    }

    /// Lines of a combined diff longer than the screen end in `…` and
    /// scroll sideways like code.
    #[test]
    fn a_long_combined_line_is_cut_and_scrolls() {
        let long = format!("++{}END", "x".repeat(150));
        let input = format!(
            "diff --cc f.txt\nindex 1,2..3\n--- a/f.txt\n+++ b/f.txt\n@@@ -1,1 -1,1 +1,1 @@@\n- a\n -b\n{long}\n"
        );
        let mut app = piped_app(input.as_bytes(), 80, 12);
        let row = |app: &mut App| {
            screen(app)
                .lines()
                .find(|l| l.contains("xxx"))
                .map(str::to_owned)
                .unwrap_or_default()
        };
        let before = row(&mut app);
        assert!(before.ends_with('…') && !before.contains("END"), "{before}");
        for _ in 0..10 {
            key(&mut app, '>');
        }
        let after = row(&mut app);
        assert!(after.starts_with('…') && after.contains("END"), "{after}");
    }

    /// A viewer of `FILES`, all counted.
    fn files_app(width: usize, height: usize) -> App {
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "range".to_owned(),
            None,
            (width, height),
            Indicators::Classic,
        );
        for (path, _, _) in FILES {
            app.event(Event::Enumerated(diff(path).change));
        }
        app.event(Event::EnumerationDone(None));
        let counts = FILES
            .iter()
            .enumerate()
            .map(|(i, &(_, added, removed))| (i, app.version(i), view::Counts { added, removed }))
            .collect();
        app.event(Event::Diffed(Done::Counts { counts }));
        app
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            key(app, c);
        }
    }

    /// The picker's rows as markup: matched characters are `<hint>`.
    fn picker_markup(app: &mut App) -> String {
        let (width, height) = (app.screen.width(), app.screen.height());
        let picker = app.picker.as_mut().expect("the picker is open");
        picker
            .rows(
                &app.doc.files,
                &app.doc.commits,
                (width, height - 1),
                picker::Progress::Done,
            )
            .iter()
            .map(|(c, selected)| {
                let mark = if *selected { ">" } else { " " };
                format!("{mark}{}", c.markup().trim_end())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `o` lists every file; typing narrows it, file names first, marking
    /// what matched; `j` is typed, not a move; `enter` opens the pick.
    #[test]
    fn the_picker_filters_as_you_type_and_opens_the_pick() {
        let mut app = files_app(60, 10);
        key(&mut app, 'o');
        insta::assert_snapshot!(screen(&mut app), @r"
        > ▏ type to filter · enter open · esc close        4 files
        M .clang-format                                  +12     -3
        M …ntation/admin-guide/kernel-parameters.txt   +1234    -56
        M …t/wireless/realtek/rtw89/rtw8852c_table.c  +54321 -12345
        M kernel/sched/fair.c                         +98765 -43210




          range   unified         / search · tab tree · ␣ commands
        ");
        typed(&mut app, "fair");
        insta::assert_snapshot!(picker_markup(&mut app), @r"
         <hint> > </>fair<hint>▏</>                                          <dim>1/4 files</>
        > <warn>M</> kernel/sched/<hint>fair</>.c                         <added>+98765</> <removed>-43210</>
        ");
        typed(&mut app, "j");
        insta::assert_snapshot!(picker_markup(&mut app), @r"
         <hint> > </>fairj<hint>▏</>                                         <dim>0/4 files</>
         <dim>   no file matches</>
        ");
        app.event(Event::Input(Input::Key(Key::Backspace, Mods::NONE)));
        app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
        assert!(app.picker.is_none());
        assert_eq!(app.doc.pos.file, 3, "kernel/sched/fair.c");
        // A tap on a row opens that file.
        key(&mut app, 'o');
        app.draw();
        app.event(Event::Input(Input::Click { x: 10, y: 1 }));
        assert!(app.picker.is_none());
        assert_eq!(app.doc.pos.file, 0);
    }

    /// Under 100 columns the title is `‹ i/N name ›`: taps on the arrows
    /// move between files, a tap on the name opens the picker, each at its
    /// exact columns.
    #[test]
    fn the_narrow_title_switches_files_with_taps() {
        let mut app = files_app(50, 10);
        let title = |app: &mut App| screen(app).lines().next().unwrap_or_default().to_owned();
        insta::assert_snapshot!(title(&mut app), @" ‹ 1/4 .clang-format › +12 -3");
        // The ranges as last drawn: the name's width moves the `›`.
        let hits = |app: &App| app.switcher.clone().expect("drawn").1;
        let tap = |app: &mut App, x: usize| {
            app.event(Event::Input(Input::Click {
                x: u16::try_from(x).expect("column"),
                y: 0,
            }));
            app.draw();
        };
        // Both cells beside an arrow are its padding: still the arrow.
        let [_, _, next] = hits(&app);
        tap(&mut app, next.start);
        assert_eq!(app.doc.pos.file, 1);
        let [_, _, next] = hits(&app);
        tap(&mut app, next.end - 1);
        assert_eq!(app.doc.pos.file, 2);
        let [prev, _, _] = hits(&app);
        tap(&mut app, prev.start);
        assert_eq!(app.doc.pos.file, 1);
        insta::assert_snapshot!(title(&mut app), @" ‹ 2/4 kernel-parameters.txt › +1234 -56");
        let [_, name, next] = hits(&app);
        tap(&mut app, next.end);
        assert!(app.picker.is_none(), "past the arrow");
        tap(&mut app, name.start);
        assert!(app.picker.is_some());
    }

    /// On a narrow screen the tree covers the whole diff area. A tap on a
    /// folder row toggles it, a tap on a file row opens the file and closes
    /// the tree, and the wheel over it scrolls the tree, not the diff.
    #[test]
    fn taps_and_the_wheel_on_the_narrow_tree_act_on_the_row_under_them() {
        let mut app = files_app(60, 7);
        key(&mut app, 't');
        let shown = screen(&mut app);
        let rows: Vec<&str> = shown.lines().collect();
        assert!(rows[1].starts_with("┌ Files · 4"), "{shown}");
        // Down to the status row, which stays.
        assert!(
            rows[2..6]
                .iter()
                .all(|r| r.starts_with('│') || r.starts_with('└')),
            "{shown}"
        );
        // The panel shows 3 rows of 7; the wheel scrolls it alone, 3 rows
        // a notch.
        let pos = app.doc.pos;
        app.event(Event::Input(Input::Scroll {
            delta: 1,
            x: 10,
            y: 3,
        }));
        let scrolled = screen(&mut app);
        assert_eq!(app.doc.pos, pos);
        assert!(
            scrolled
                .lines()
                .nth(2)
                .is_some_and(|r| r.contains("rtw89/")),
            "{scrolled}"
        );
        app.event(Event::Input(Input::Scroll {
            delta: -1,
            x: 10,
            y: 3,
        }));
        // Row 2 is the first row inside the border: a folder; its last row,
        // the bottom edge of the panel, is the border, which does nothing.
        app.event(Event::Input(Input::Click { x: 10, y: 3 }));
        let shown = screen(&mut app);
        assert!(shown.contains("▶ Documentation/admin-guide/"), "{shown}");
        app.event(Event::Input(Input::Click { x: 10, y: 5 }));
        assert!(app.tree_shown, "the border isn't a row");
        app.event(Event::Input(Input::Click { x: 10, y: 3 }));
        assert!(screen(&mut app).contains("▼ Documentation/admin-guide/"));
        // The first row: `.clang-format`, a file.
        app.event(Event::Input(Input::Click { x: 3, y: 2 }));
        assert!(!app.tree_shown, "a picked file closes the tree");
        assert_eq!(app.doc.pos.file, 0);
    }

    #[test]
    fn plus_and_minus_in_the_tree_widen_and_narrow_it() {
        let mut app = app(120, 20);
        let width = app.side_width();
        app.event(Event::Input(Input::Key(Key::Tab, Mods::NONE)));
        key(&mut app, '+');
        assert_eq!(app.side_width(), width.map(|w| w + TREE_STEP));
        for _ in 0..40 {
            key(&mut app, '+');
        }
        assert_eq!(app.side_width(), Some(120 - MIN_DIFF));
        for _ in 0..40 {
            key(&mut app, '-');
        }
        assert_eq!(app.side_width(), Some(MIN_TREE));
        assert!(
            screen(&mut app)
                .lines()
                .next()
                .is_some_and(|l| l.starts_with("┌ Files"))
        );
    }

    /// In the rendered view the title counts what the tree counts: the
    /// file's changed source lines, not its changed rendered lines.
    #[test]
    fn the_rendered_view_titles_the_trees_counts() {
        let side = Some(Side::Worktree {
            mode: Mode::File,
            id: None,
        });
        let old = Text::new("Intro.\n\nGone para here.\n\nBeta three four.\n\nTail.\n".into());
        let new = Text::new("Intro.\n\nBeta three FOUR.\n\nTail.\n".into());
        let fd = FileDiff {
            change: FileChange {
                path: "README.md".into(),
                status: Status::Modified,
                old: side,
                new: side,
                staging: None,
            },
            hunks: hunks(&old, &new, &Options::default()),
            old,
            new,
            binary: false,
            too_large: false,
            note: None,
        };
        let mut config = Config::default();
        config.set("render", "md:fake").expect("valid");
        let mut app = app_of(&config, "working tree", fd, (120, 20));
        let para = |source: usize, text: &str| plugin::Block {
            kind: "paragraph".to_owned(),
            source: source..source + 1,
            lines: [text, ""]
                .map(|text| plugin::Line {
                    text: text.to_owned(),
                    paint: Vec::new(),
                    links: Vec::new(),
                })
                .to_vec(),
            language: None,
        };
        let sides = [
            plugin::Side {
                blocks: vec![
                    para(0, "Intro."),
                    para(2, "Gone para here."),
                    para(4, "Beta three four."),
                    para(6, "Tail."),
                ],
            },
            plugin::Side {
                blocks: vec![
                    para(0, "Intro."),
                    para(2, "Beta three FOUR."),
                    para(4, "Tail."),
                ],
            },
        ];
        let diff = Arc::clone(app.doc.diff_of(0).expect("diffed"));
        let rendered = view::rendered::build(Arc::clone(&diff), sides, crate::grammars(), 3, 60);
        assert_eq!((rendered.added, rendered.removed), (1, 3));
        app.event(Event::Diffed(Done::Rendered {
            file: 0,
            version: app.version(0),
            id: app.render_id,
            diff,
            result: Ok(Box::new(rendered)),
        }));
        app.doc.set_rendered_mode(true);
        let shown = screen(&mut app);
        assert!(shown.contains("Beta three FOUR."), "{shown}");
        let title = shown.lines().next().expect("the title row");
        let tree = shown
            .lines()
            .find(|l| l.contains("README.md") && l.contains('│'))
            .expect("the tree's row");
        assert!(title.contains("+1 -3"), "{title}");
        assert!(tree.contains("+1") && tree.contains("-3"), "{tree}");
    }

    /// A `render` setting alone looks nothing up: drawing and moving
    /// between files of the renderer's type ask for no program, and no
    /// render job is queued, until `r` asks for the rendered view.
    #[test]
    fn renderers_are_looked_up_only_when_r_asks() {
        let mut config = Config::default();
        config.set("render", "rs:nothere").expect("valid");
        let mut app = app_of(&config, "working tree", diff("src/a.rs"), (120, 20));
        for c in ['f', 'F', 'n', 'N', 'j'] {
            screen(&mut app);
            key(&mut app, c);
        }
        screen(&mut app);
        assert_eq!(app.programs.len(), 0);
        assert!(!app.jobs().iter().any(|j| matches!(j, Job::Render(_))));
        key(&mut app, 'r');
        assert_eq!(app.programs.len(), 1, "looked up once r asked");
        assert!(!app.doc.rendered, "not on without the program");
    }

    /// Wrapped, a file left part way down a long row comes back at that
    /// same screen row of it, not at the row's first.
    #[test]
    fn a_place_inside_a_wrapped_row_is_kept_across_files() {
        let old = (1..=60)
            .map(|i| format!("line {i}\n"))
            .collect::<Vec<_>>()
            .concat();
        let long = "a line long enough to wrap ".repeat(10);
        let new = old.replace("line 4\n", &format!("line 4 {long}\n"));
        let (old, new) = (Text::new(old.into()), Text::new(new.into()));
        let fd = FileDiff {
            hunks: hunks(&old, &new, &Options::default()),
            old,
            new,
            ..diff("src/a.rs")
        };
        let mut config = Config::default();
        config.apply_source("wrap = true\n", &mut Vec::new());
        let mut app = app_of(&config, "working tree", fd, (60, 10));
        for _ in 0..20 {
            if app.doc.pos.offset > 0 {
                break;
            }
            key(&mut app, 'j');
        }
        assert!(app.doc.pos.offset > 0, "{:?}", app.doc.pos);
        let (pos, before) = (app.doc.pos, screen(&mut app));
        key(&mut app, 'j');
        assert_ne!(app.doc.pos, pos, "not at the bottom");
        key(&mut app, 'k');
        key(&mut app, 'f');
        assert_eq!(app.doc.pos.file, 1);
        key(&mut app, 'F');
        assert_eq!(app.doc.pos, pos);
        let after = screen(&mut app);
        assert_eq!(after, before);
        insta::assert_snapshot!("wrapped_place_kept", after);
    }

    /// Thirty C functions, the 15th and 16th swapped: lines removed and
    /// added to the line diff, code moved to the syntax diff.
    fn moved_diff() -> FileDiff {
        let f = |i: usize| format!("int f{i}(void) {{ return {i}; }}\n");
        let old: String = (1..=30).map(f).collect();
        let new: String = (1..=30)
            .map(|i| match i {
                15 => f(16),
                16 => f(15),
                _ => f(i),
            })
            .collect();
        let (old, new) = (Text::new(old.into()), Text::new(new.into()));
        FileDiff {
            hunks: hunks(&old, &new, &Options::default()),
            old,
            new,
            ..diff("src/f.c")
        }
    }

    /// `d` between the line diff and the syntax diff keeps the view where
    /// it was in the file's rows.
    #[test]
    fn the_syntax_diff_toggle_keeps_the_place() {
        let mut app = app_of(&Config::default(), "working tree", moved_diff(), (100, 10));
        for _ in 0..4 {
            key(&mut app, 'j');
        }
        let lines = screen(&mut app);
        key(&mut app, 'd');
        assert!(app.doc.structural);
        app.draw();
        for job in app.jobs() {
            if let Job::Structural {
                file,
                version,
                diff,
            } = job
            {
                app.event(Event::Diffed(Done::Structural {
                    file,
                    version,
                    result: print::structural_diff(crate::grammars(), &diff, None),
                    diff,
                }));
            }
        }
        let syntax = screen(&mut app);
        assert!(syntax.contains("»│"), "the syntax diff shows:\n{syntax}");
        insta::assert_snapshot!("syntax_diff_on", syntax);
        key(&mut app, 'd');
        let off = screen(&mut app);
        assert_eq!(off, lines);
        insta::assert_snapshot!("syntax_diff_off", off);
    }

    /// The screen with each run of cells in one of `roles`' styles in
    /// `<role>…</>`.
    fn role_markup(app: &mut App, roles: &[Role]) -> String {
        app.draw();
        let styles: Vec<_> = roles.iter().map(|&r| (app.theme.style(r), r)).collect();
        let grid = app.screen.shown_grid();
        (0..grid.height())
            .map(|y| {
                let mut line = String::new();
                let mut current = None;
                for cell in grid.row(y) {
                    let role = styles
                        .iter()
                        .find(|(style, _)| *style == cell.style())
                        .map(|&(_, r)| r);
                    if role != current {
                        if current.is_some() {
                            line.push_str("</>");
                        }
                        if let Some(r) = role {
                            line.extend(["<", r.key(), ">"]);
                        }
                        current = role;
                    }
                    line.push_str(cell.text());
                }
                if current.is_some() {
                    line.push_str("</>");
                }
                line.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `git show` of a merge in the viewer: the commit's header, metadata
    /// and message in their colors, then the combined diff's lines colored
    /// by their prefix columns.
    #[test]
    fn a_piped_merge_shows_its_commit_and_combined_diff_in_their_colors() {
        let merge = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/git-show-merge.diff"
        ));
        let roles = [
            Role::CommitHeader,
            Role::Dim,
            Role::HunkHeader,
            Role::Added,
            Role::Removed,
        ];
        let mut app = piped_app(merge, 100, 14);
        // The default theme shares colors between these roles and others
        // (`warn` is yellow too): underlined, each is told apart.
        app.theme = Theme::parse(
            "roles",
            "commit-header = yellow underline\ndim = dim underline\n\
             hunk-header = cyan underline\nadded = green underline\nremoved = red underline\n",
            &mut Vec::new(),
        );
        key(&mut app, 't');
        read_back(&mut app);
        insta::assert_snapshot!("piped_merge_commit", role_markup(&mut app, &roles));
        key(&mut app, 'f');
        read_back(&mut app);
        insta::assert_snapshot!("piped_merge_combined", role_markup(&mut app, &roles));
    }

    /// `view = auto` splits from `split-min-width` columns of screen, 140
    /// by default, and is unified a column short of it.
    #[test]
    fn auto_splits_from_the_split_min_width() {
        let mut app = app(140, 12);
        assert!(app.doc.layout.split);
        let split = screen(&mut app);
        assert!(
            split.lines().any(|l| l.matches("line 4").count() == 2),
            "side by side:\n{split}"
        );
        insta::assert_snapshot!("auto_at_split_min_width", split);
        app.resize(139, 12);
        assert!(!app.doc.layout.split);
        insta::assert_snapshot!("auto_below_split_min_width", screen(&mut app));
    }
}

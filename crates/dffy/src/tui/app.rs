//! The viewer's state and what each event does to it. Nothing here touches
//! the terminal, threads or processes: the loop in [`super::run`] owns
//! those and carries out the [`Action`]s returned here.
//!
//! Keys go to one of two panes, the diff or the left panel; `tab` switches.
//! The left panel shows the file tree, or after `i` the comment threads.
//! On wide terminals it's beside the diff, on narrow ones (phones
//! included) an overlay over it that closes once a file is picked.
//! Every action has a key that needs neither ctrl nor a symbol, since
//! phone keyboards put symbols a layer away; mouse clicks and taps are
//! shortcuts only. Keys map to actions through the config's bindings.
//!
//! While lines are selected (`v`), moving moves the selection's end
//! instead of the view, `s` picks the side copied, and any other action
//! ends the selection first.

mod command_panel;
mod conflict;
mod control;
mod draw;
mod mouse;
mod review;
mod search;
mod staging;
mod threads;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::ops::{ControlFlow, Range};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dffy_core::annotation::Anchor;
use dffy_core::bstr::BString;
use dffy_core::conflict::Choices;
use dffy_core::diff::Whitespace;
use dffy_core::document::{File, Missing, Needle, Pos, Render, Side, Source};
use dffy_core::git::{Edit, FileChange, FileDiff, Picked, Revisions, Status, Update};
use dffy_core::highlight::Language;
use dffy_core::watch::Notice;

use super::chrome::{self};
use super::commands::Commands;
use super::comments::{self, Panel, Writes};
use super::help::Help;
use super::menu::{self, Menu};
use super::picker::Picker;
use super::piped;
use super::tree::{self, Item, Tree};
use super::view::{Doc, Hit};
use super::worker::{Counted, Done, Job, PlaceJob, RenderJob};
use super::{Event, view};
use crate::config::{
    self, Background, Config, Context, DefaultDiff, Indicators, ModifiedLines, StatusBar, TreeMode,
    View,
};
use crate::control::viewer::Reply;
use crate::daemon::client;
use crate::keys::{Action as Act, Keys};
use crate::plugin::agent::Said as AgentSaid;
use crate::plugin::review::Naming;
use crate::plugin::{self, Program, Renderers};
use crate::print;
use crate::render::{Layout, escape, thousands};
use crate::review::ask::Asking;
use crate::review::model::compose::Compose;
use crate::review::model::{self, Comments, Hint, Part};
use crate::review::prompt;
use crate::review::session::{self, Reviewing};
use crate::term::{self, Input, Key, Mods, Screen};
use crate::theme::{self, Theme};

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
/// Said to a git operation asked for while another runs.
const STILL_EDITING: &str = "still applying; wait";
/// Said to a write to the comments while a post runs.
const POSTING: &str = "posting; edit once it's done";
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
    /// Put where this repository-relative path is on disk on the
    /// clipboard, from the filesystem's root.
    CopyFullPath(BString),
    /// Turn the terminal's mouse reports on or off.
    Mouse(bool),
    /// Diff again with these settings: the files were marked stale.
    Rediff {
        highlight: bool,
        whitespace: Whitespace,
    },
    /// Write a comment in the editor, starting from this text; the text
    /// written goes back through [`App::composed`].
    Compose(String),
    /// Open this http(s) link with the platform's opener.
    Browse(String),
    /// Open a tab in the user's terminal at the working tree's root; what
    /// came of it arrives as [`Event::Terminal`].
    OpenTerminal,
    /// Ask the review plugin `request`; what it says comes back as
    /// [`Event::Review`] under `id`. A `prepare`, `pull` or `post` cancels
    /// the one before it.
    Review {
        id: u64,
        request: plugin::review::Request,
    },
    /// List `revisions` from now on, when given, reading `comments` when
    /// it's another file. `moved` is the review's head before and after, to
    /// count commits.
    Retarget {
        revisions: Option<Revisions>,
        comments: Option<PathBuf>,
        moved: Option<(String, String)>,
    },
    /// Stage, unstage or discard `picked` lines of `diff`, as shown; what
    /// came of it arrives as [`Event::Edited`].
    EditLines {
        diff: Arc<FileDiff>,
        picked: Picked,
        edit: Edit,
    },
    /// Work out what discarding `picked` lines of `diff` takes, for the
    /// question before it; that arrives as [`Event::Planned`].
    PlanDiscard {
        diff: Arc<FileDiff>,
        picked: Picked,
    },
    /// Write what `choices` resolve the conflicted working-tree file
    /// `path` to; what came of it arrives as [`Event::Resolved`].
    WriteResolution {
        path: BString,
        choices: Arc<Choices>,
    },
    /// `git add` the working-tree file `path`, marking its conflict
    /// resolved; what came of it arrives as [`Event::Edited`].
    GitAdd(BString),
    /// Put a thread's suggestion in place of its lines in the working tree;
    /// what came of it arrives as [`Event::Edited`].
    ApplySuggestion(Box<Suggestion>),
}

/// A suggestion to apply, as [`dffy_core::annotation::apply_suggestion`]
/// takes it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Suggestion {
    /// The diff shown of the file the thread is on.
    pub diff: Arc<FileDiff>,
    /// The first of the thread's lines on the diff's new side, 0-based.
    pub at: usize,
    pub anchor: Anchor,
    /// The lines that go in their place; none deletes them.
    pub lines: Vec<String>,
    /// The review checked out: HEAD must be its head.
    pub checkout: Option<Checkout>,
    /// The thread's first comment, and the comment the suggestion is of.
    pub thread: String,
    pub comment: String,
}

/// A suggestion being applied: what's said of it once it is, and the
/// control API's request waiting on it.
struct Applying {
    thread: String,
    comment: String,
    path: String,
    lines: usize,
    reply: Option<Reply>,
}

/// A review session's head, which HEAD must be to change the working tree
/// for it: its changes are there only once it's checked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Checkout {
    /// What the review is called, as `PR #7`.
    pub label: String,
    pub head: String,
}

/// A thread handed to the user's own coding agent, which the loop starts
/// interactively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Handoff {
    /// The thread's first comment.
    pub thread: String,
    /// The comments file it's in.
    pub file: PathBuf,
    /// That file is the review session's.
    pub session: bool,
    pub checkout: Option<Checkout>,
}

/// What saving the text box does after, with the thread it's in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum OnSave {
    #[default]
    Nothing,
    Ask,
    Fix,
}

/// Which uncommitted changes the diff shows: the diffs whose lines can be
/// staged, unstaged and discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Uncommitted {
    /// Plain `dffy`: `HEAD` against the working tree, each file staged,
    /// unstaged or partly.
    Plain,
    /// `--staged`: `HEAD` against the index.
    Staged,
    /// `--unstaged`: the index against the working tree.
    Unstaged,
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

/// The thread to focus in a file once its threads are placed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seek {
    /// Its first (`m`), or with `true` its last (`M`).
    Edge(bool),
    /// The one with this root, selected in the comments panel.
    Root(String),
    /// The one with this root, just written: focused where it is.
    Saved(String),
}

/// Where keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Diff,
    /// The left panel: the tree, or the comments while [`App::panel`] is
    /// open.
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
    menu: Option<Menu>,
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
    /// The comment last clicked, by its thread's root and its id, which
    /// `copy_comment` copies.
    tapped_comment: Option<(String, String)>,
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
    /// The tree's first row when the count pass last started, to start it
    /// again with the files scrolled into view first.
    count_scroll: Option<usize>,
    /// Each language a listed file has, by path, in the order they first
    /// came, and how many of them went to the pool to compile ahead.
    languages: Vec<Language>,
    prepared: usize,
    help: Option<Help>,
    picker: Option<Picker>,
    /// The review comments; `None` without a comments file (a piped diff,
    /// paths compared outside git without `--comments`).
    comments: Option<Comments>,
    /// The local review's file, plain `dffy`'s: a review session's
    /// comments file shows its drafts too.
    local_comments: Option<PathBuf>,
    /// The comment threads, shown in the left panel in place of the tree.
    panel: Option<Panel>,
    /// What the text box's or the editor's text becomes, while either is
    /// open.
    composing: Option<Compose>,
    /// The text box's text and where it wrote, while the editor has it:
    /// the box opens again with it when the editor fails.
    handed: Option<(String, Writes)>,
    /// A box's text whose thread is gone, for the loop to keep in a file.
    orphaned: Option<String>,
    /// `d` was pressed once on the focused thread; another deletes.
    deleting: bool,
    /// Replying to an outdated thread said once the plugin's warning.
    warned_outdated: bool,
    /// A file gone to for a thread before its threads were placed, and
    /// which of them to focus once they are.
    thread_pending: Option<(usize, Seek)>,
    /// The comments changed since the pool last took their annotations.
    annotations_changed: bool,
    /// The review plugin's session: the plugin, the review shown, and what
    /// runs.
    reviewing: Reviewing,
    /// The threads asked about an agent, and what they wait on.
    asking: Asking,
    /// Asks for the agent plugin, and asks to stop, by id, for the loop to
    /// take once a frame is out.
    agent_asks: Vec<(u64, plugin::agent::Request)>,
    agent_stops: Vec<u64>,
    /// The ends of asks that came while a post runs, by id, in order: what
    /// they write waits for the post to end.
    held_agents: Vec<(u64, AgentSaid)>,
    /// What saving the text box open does to its thread after.
    on_save: OnSave,
    /// A thread to hand to the user's coding agent, for the loop to take
    /// once a frame is out.
    handoff: Option<Handoff>,
    /// The suggestion being applied.
    applying: Option<Applying>,
    /// The reviews to pick from.
    reviews: Option<review::Picker>,
    /// The dialog that posts the review.
    posting: Option<review::Dialog>,
    /// Threads an update brought, by root, in list order: `m` goes to
    /// them first.
    new_threads: Vec<String>,
    /// A quit was asked for while a post runs; the next one stops it.
    quit_armed: bool,
    /// The viewer is stopping: what's being written is saved, even while a
    /// post runs.
    leaving: bool,
    /// When the last key or click came: you're idle after a while without
    /// one, which the review plugin is told.
    last_input: Instant,
    /// Inputs a person made so far: keys, pastes, presses and wheel steps,
    /// not a release or a drag ending a press already counted.
    inputs: u64,
    /// [`App::inputs`] when `D` asked the loop for its discard's plan: the
    /// plan asks its question only if nothing came since.
    discard_asked: Option<u64>,
    /// The current branch, which new local comments go on; `None` on a
    /// detached HEAD.
    branch: Option<String>,
    /// The text box opened again after its save failed: `esc` now
    /// discards it.
    save_failed: bool,
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
    /// A finished post or a failure the terminal reports until the next
    /// key or click, which means the user has seen it.
    unseen: Option<term::Status>,
    /// The program status last sent to the terminal.
    reported: Option<term::Status>,
    /// What the window title last sent names.
    titled: Option<String>,
    /// The working tree's directory name, for the window title.
    repository: Option<String>,
    /// What the repository's diff selects, to word an empty list.
    selected: Option<(Revisions, DefaultDiff)>,
    /// The diff the viewer started on is of uncommitted changes, lines of
    /// which can be staged, unstaged and discarded.
    uncommitted: Option<Uncommitted>,
    /// A question in the status bar waiting for `y`, what `y` does, and
    /// what's said when any other input drops it.
    confirm: Option<(String, Action, &'static str)>,
    /// A git operation on the working tree or the index went to the loop
    /// and hasn't said how it went: one runs at a time, so two quick
    /// presses can't race on the same lines.
    editing: bool,
    /// The conflict `J`, `K` or a choice last went to: its file and region.
    conflict: Option<(usize, usize)>,
    /// The conflict a choice was made on, whose file's diff is on its way:
    /// once it's loaded, the view goes back to it.
    conflict_anchor: Option<(usize, usize)>,
    /// What the last press let go was on, and the file shown then, which a
    /// shift-click selects from when nothing is selected.
    anchor: Option<(usize, Hit)>,
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
    /// The diff's rows on screen still show what they showed when drawn:
    /// nothing drew over them, and only scrolls of the diff happened since.
    /// The next frame copies the ones still on screen instead of laying
    /// them out again.
    kept: bool,
    /// The event being handled scrolled the diff.
    scrolled: bool,
    /// The control API's requests waiting on this view, and its
    /// subscribers.
    control: control::Control,
}

impl App {
    /// A viewer showing `first`, or nothing yet in watch mode. `indicators`
    /// is the config's, already resolved for color.
    #[expect(clippy::too_many_lines, reason = "a line per field of the state")]
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
            unmerged: false,
            digits: 0,
            inline_signs: false,
            scroll: 0,
            line_numbers: config.line_numbers,
            hunk_headers: config.hunk_headers,
            modified_lines: config.modified_lines == ModifiedLines::Changed,
        };
        let mut doc = Doc::new(placeholder, height);
        doc.document.structural = config.syntax_diff;
        doc.set_rendered_mode(config.rendered);
        doc.set_highlight(config.highlight);
        doc.document.full = config.context == Context::Full;
        doc.overview = true;
        let mut tree = Tree::default();
        let languages = first
            .iter()
            .filter_map(|f| crate::grammars().detect(&f.path, &[]))
            .cloned()
            .collect();
        if let Some(first) = first {
            tree.insert(0, &first.path);
            doc.document.push(first, Source::Repo);
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
            menu: None,
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
            tapped_comment: None,
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
            count_scroll: None,
            languages,
            prepared: 0,
            help: None,
            picker: None,
            comments: None,
            local_comments: None,
            panel: None,
            composing: None,
            handed: None,
            orphaned: None,
            deleting: false,
            warned_outdated: false,
            thread_pending: None,
            annotations_changed: false,
            reviewing: Reviewing::new(config.review.clone()),
            asking: Asking::default(),
            agent_asks: Vec::new(),
            agent_stops: Vec::new(),
            held_agents: Vec::new(),
            on_save: OnSave::Nothing,
            handoff: None,
            applying: None,
            reviews: None,
            posting: None,
            new_threads: Vec::new(),
            quit_armed: false,
            leaving: false,
            last_input: Instant::now(),
            inputs: 0,
            discard_asked: None,
            branch: None,
            save_failed: false,
            switcher: None,
            tree_button: None,
            message: None,
            tip: false,
            enumerating: true,
            unseen: None,
            reported: None,
            titled: None,
            repository: None,
            selected: None,
            uncommitted: None,
            confirm: None,
            editing: false,
            conflict: None,
            conflict_anchor: None,
            anchor: None,
            others: None,
            piped: false,
            watch: Watch::Off,
            focus: None,
            frame: 0,
            visible: 0..1,
            dirty: true,
            kept: false,
            scrolled: false,
            control: control::Control::default(),
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
        let (Some(focus), Some(file)) =
            (&self.focus, self.doc.document.files().len().checked_sub(1))
        else {
            return;
        };
        let path = &self.doc.document.files()[file].change().path;
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
        let files = self.doc.document.files();
        files.get(file).map_or(u64::MAX, view::Entry::version)
    }

    /// What the repository's diff selects, and the default it was picked
    /// with, for the words of an empty list.
    pub(crate) fn set_selected(&mut self, revisions: Revisions, default: DefaultDiff) {
        self.selected = Some((revisions, default));
    }

    /// The diff is of uncommitted changes, as `which` shows them.
    pub(crate) fn set_uncommitted(&mut self, which: Option<Uncommitted>) {
        self.uncommitted = which;
    }

    pub(crate) fn set_repository(&mut self, name: Option<String>) {
        self.repository = name;
    }

    /// The files come from a piped diff: they keep its order.
    pub(crate) fn set_piped(&mut self) {
        self.piped = true;
    }

    /// With color off the theme is attributes only, and stays so.
    pub(crate) fn set_color(&mut self, color: bool) {
        self.color = color;
    }

    /// Redraws: the naming can arrive with the command panel open.
    pub(crate) fn set_naming(&mut self, naming: Option<Naming>) {
        self.reviewing.set_naming(naming);
        self.dirty = true;
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

    /// `words` as one line, with the keys bound here.
    fn say(&self, words: &[Part]) -> String {
        model::say(words, |hint| hint_words(&self.keys, hint))
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
            self.doc.document.files().len() + piped::READ_ALL_AHEAD
        } else {
            self.doc.document.pos.file + piped::READ_AHEAD
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
            unmerged: false,
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
        self.doc.height = self.screen.height().saturating_sub(2);
        self.doc.set_layout(layout);
        self.tree.set_height(self.tree_rows().1);
        self.dirty = true;
        self.kept = false;
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
    /// Also after the editor, which may have reported a status and set a
    /// title of its own.
    pub(crate) fn invalidate(&mut self) {
        self.screen.invalidate();
        self.dirty = true;
        self.kept = false;
        self.reported = None;
        self.titled = None;
    }

    /// The window title and the program status report (OSC 7501), those
    /// that changed since they were last sent.
    pub(crate) fn status_report(&mut self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        if self.titled.as_ref() != Some(&self.description) {
            let shown = self.description.as_str();
            out = match &self.repository {
                Some(name) => term::window_title(&[name, shown]),
                None => term::window_title(&[shown]),
            };
            self.titled = Some(self.description.clone());
        }
        let status = self
            .review_status()
            .or_else(|| self.unseen.clone())
            .unwrap_or_else(|| {
                // A piped diff is read only as far as the viewer goes, so it
                // can stay unfinished for as long as dffy runs.
                if self.enumerating && !self.piped {
                    term::Status {
                        state: term::State::Working,
                        message: Some("Listing changes".to_owned()),
                        ..term::Status::default()
                    }
                } else {
                    term::Status::default()
                }
            });
        if self.reported.as_ref() != Some(&status) {
            out.extend_from_slice(&status.report());
            self.reported = Some(status);
        }
        (!out.is_empty()).then_some(out)
    }

    /// Puts `text` in the status bar. It may hold a plugin's or a file's
    /// words, so it's escaped here, once, rather than at every frame.
    pub(crate) fn message(&mut self, text: impl Into<String>) {
        let text = escape(text.into().as_bytes());
        self.message = Some((text, Instant::now() + MESSAGE_TIME));
        self.tip = false;
        self.dirty = true;
    }

    /// A failure's note, which the terminal also hears about: the note is
    /// gone in seconds, and the user may be in another tab.
    fn failed(&mut self, text: String) {
        self.unseen = Some(term::Status {
            state: term::State::Error,
            message: Some(text.clone()),
            ..term::Status::default()
        });
        self.message(text);
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
            .chain(self.reviewing.idle_deadline(self.last_input))
            .chain(self.reviewing.reload_deadline())
            .chain(self.reviewing.plan_deadline(self.comments.as_ref()))
            .chain(self.control.deadline())
            .min()
    }

    pub(crate) fn mouse(&self) -> bool {
        self.mouse
    }

    pub(crate) fn event(&mut self, event: Event) -> Option<Action> {
        let kept = std::mem::take(&mut self.kept);
        self.scrolled = false;
        if let Event::Input(input) = &event
            && !matches!(
                input,
                Input::Release { .. } | Input::Drag { .. } | Input::Background { .. }
            )
        {
            self.inputs += 1;
        }
        if matches!(event, Event::Input(_)) {
            self.last_input = Instant::now();
            self.reviewing.input();
        }
        let mut action = self.handle(event);
        self.release_held_agents();
        if matches!(
            action,
            Some(
                Action::EditLines { .. }
                    | Action::PlanDiscard { .. }
                    | Action::WriteResolution { .. }
                    | Action::GitAdd(_)
                    | Action::ApplySuggestion(_)
            )
        ) {
            if self.editing {
                self.message(STILL_EDITING);
                action = None;
            } else {
                self.editing = true;
                if matches!(action, Some(Action::PlanDiscard { .. })) {
                    self.discard_asked = Some(self.inputs);
                }
            }
        }
        self.sync_threads();
        self.control_settle();
        self.kept = kept && self.scrolled;
        // Stopping a post halfway leaves the review half sent until the
        // next post, so the first quit asks again.
        if !self.posting_now() {
            self.quit_armed = false;
        } else if action == Some(Action::Quit)
            && !std::mem::replace(&mut self.quit_armed, true)
            && let Some(note) = self.reviewing.still_posting()
        {
            self.message(self.say(&note));
            action = None;
        }
        action
    }

    /// The viewer is stopping, a post it waited for over or given up on:
    /// the text box's text is saved, as `ctrl+c` saves it, and the post
    /// dialog's summary.
    pub(crate) fn interrupted(&mut self) {
        self.leaving = true;
        self.close_post();
        self.close_box();
        self.release_held_agents();
    }

    /// What asks said while a post ran, now that writes go on.
    fn release_held_agents(&mut self) {
        if self.held_agents.is_empty() || self.writes_held() {
            return;
        }
        for said in std::mem::take(&mut self.held_agents) {
            let effects = self.asking.said(said, self.comments.as_mut());
            self.review_effects(effects);
        }
    }

    /// Gives the diff the threads of the file shown, as placed now.
    fn sync_threads(&mut self) {
        let file = self.doc.document.pos.file;
        let threads = self
            .comments
            .as_ref()
            .zip(self.doc.document.files().get(file))
            .and_then(|(c, e)| {
                if e.is_page() {
                    Some(c.conversation())
                } else {
                    c.threads(&e.change().path)
                }
            })
            .unwrap_or_default();
        let again = self.doc.threads_file() == file;
        self.doc.set_threads(file, threads);
        if again {
            self.keep_orphaned_box();
        }
    }

    /// Nothing came before the deadline: a flash, a drag at the edge, a
    /// note or the review's wait may be over.
    fn tick(&mut self) -> Option<Action> {
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
        self.review_tick()
    }

    #[expect(
        clippy::too_many_lines,
        reason = "a dispatch: an arm per event or group of them"
    )]
    fn handle(&mut self, event: Event) -> Option<Action> {
        match event {
            Event::Input(input) => return self.input(input),
            Event::Enumerated(change) => {
                if let Some(lang) = crate::grammars().detect(&change.path, &[])
                    && !self.languages.contains(lang)
                {
                    self.languages.push(lang.clone());
                }
                let file = self.doc.document.files().len();
                self.tree.insert(file, &change.path);
                self.doc.document.push(change, Source::Repo);
                if let Some(comments) = &mut self.comments {
                    comments.locate_one(file, &self.doc.document.files()[file]);
                }
                self.check_focus();
                self.dirty = true;
            }
            Event::Piped(item) => {
                let file = self.doc.document.files().len();
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
                        self.doc.document.push(change, Source::Commit(commit));
                    }
                    piped::Item::File(change, piped) => {
                        self.tree.insert(file, &change.path);
                        self.doc.document.push(change, Source::Piped(piped));
                    }
                }
                if self.doc.document.files().len() == piped::MAX_ENTRIES {
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
                    self.failed(format!("listing changes failed: {error}"));
                }
                if !self.piped && self.doc.sort_by_path() {
                    // Positions moved, so a search's marks and counts are
                    // void. Work asked for by position checks the version
                    // of the entry now there.
                    if let Some(search) = &mut self.search {
                        search.counts = vec![None; self.doc.document.files().len()];
                        search.pending = None;
                        self.doc.document.search = None;
                    }
                    self.rebuild_tree();
                    if let Some(picker) = &mut self.picker {
                        picker.rebuild(self.doc.document.files());
                    }
                }
                self.sync_conversation();
                self.count_pass = true;
                self.dirty = true;
            }
            Event::Diffed(done) => self.done(done),
            Event::Refreshed(update) => self.refreshed(&update),
            Event::RefreshFailed(error) => self.failed(format!("refreshing failed: {error}")),
            Event::Others(others) => {
                self.others = others;
                self.dirty |= self.doc.document.files().is_empty();
            }
            Event::Watch(event) => self.watch_event(event),
            event @ (Event::Edited(_) | Event::Planned { .. } | Event::Resolved { .. }) => {
                self.edit_done(event);
            }
            Event::Comments => {
                if let Some(comments) = &mut self.comments {
                    let effects = self.reviewing.file_changed(comments, Instant::now());
                    self.review_effects(effects);
                }
            }
            Event::Branch(branch) => self.branch_moved(branch),
            Event::Tick => return self.tick(),
            Event::Review { id, said } => return self.review_event(id, said),
            Event::ReviewPush(push) => self.review_pushed(push),
            Event::ReviewMoved(count) => self.review_updated(count),
            Event::ReviewRebased { last, head } => self.reviewed_head_rebased(&last, &head),
            // An ask's end writes the comments file, so it waits for a post.
            Event::Agent { id, said }
                if self.writes_held()
                    && matches!(said, AgentSaid::Answered(_) | AgentSaid::NotInstalled) =>
            {
                self.held_agents.push((id, said));
            }
            Event::Agent { id, said } => {
                let effects = self.asking.said((id, said), self.comments.as_mut());
                self.review_effects(effects);
            }
            Event::Agents(agents) => {
                let effects = self.asking.listed(agents);
                self.review_effects(effects);
            }
            Event::Control(inbound) => return self.control(inbound),
            Event::Applied(result) => self.applied(result),
            Event::HandedOff(started) => self.handed_off(started),
            Event::Terminal { dir, opened } => return self.terminal_opened(&dir, opened),
            // The loop handles these: they need the terminal, or may start
            // the plugin's `detect`.
            Event::Resize | Event::Interrupt | Event::Described(_) => {}
        }
        None
    }

    /// Takes a refreshed file list. A search's counts and marks stay with
    /// the files they're of, wherever those moved, unless they changed.
    fn refreshed(&mut self, update: &Update) {
        // By position, to find where each file went.
        let before: Vec<(BString, u64)> = if self.search.is_some() {
            self.doc
                .document
                .files()
                .iter()
                .map(|e| (e.change().path.clone(), e.version()))
                .collect()
        } else {
            Vec::new()
        };
        let marks = self.doc.document.search.take();
        self.doc.replace(update);
        if let Some(search) = &mut self.search {
            let now: HashMap<&BString, (usize, u64)> = self
                .doc
                .document
                .files()
                .iter()
                .enumerate()
                .map(|(i, e)| (&e.change().path, (i, e.version())))
                .collect();
            // Where the file at `file` went; `None` when it's gone or, with
            // `same`, changed.
            let moved = |file: usize, same: bool| {
                let (path, version) = before.get(file)?;
                let &(to, current) = now.get(path)?;
                (!same || current == *version).then_some(to)
            };
            let mut counts = vec![None; self.doc.document.files().len()];
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
            self.doc.document.search = marks.and_then(|mut m| {
                m.file = moved(m.file, true)?;
                Some(m)
            });
        }
        self.rebuild_tree();
        if let Some(picker) = &mut self.picker {
            picker.rebuild(self.doc.document.files());
        }
        // A list that was empty takes the page that waited for it.
        self.sync_conversation();
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

    /// Takes `file`'s diff, and where its comments landed on it.
    fn loaded(
        &mut self,
        file: usize,
        result: Result<Box<File>, String>,
        placed: Option<model::Landed>,
    ) {
        self.doc.document.search.take_if(|m| m.file == file);
        let chosen = self.doc.document.files()[file].choices().is_some();
        if self.doc.load(file, result) {
            self.placed(file, placed);
            self.doc.document.evict(CACHE_BYTES, &self.visible);
            self.doc.normalize();
            self.conflict_loaded(file, chosen);
            self.dirty = true;
        }
    }

    /// Takes where `file`'s comments landed on its line diff `diff`, unless
    /// it was diffed again since.
    fn placed_on(&mut self, file: usize, diff: &Arc<FileDiff>, placed: model::Landed) {
        let loaded = self.doc.document.file(file);
        if loaded.is_some_and(|l| Arc::ptr_eq(model::line_diff(l), diff)) {
            self.placed(file, Some(placed));
            self.dirty = true;
        }
    }

    /// Takes a worker's result if it's still for the file at its position:
    /// work for a file that changed or moved since it was asked for is
    /// dropped. The job for the file as it is now was asked for with the
    /// draw that followed the change, under its own key.
    fn done(&mut self, done: Done) {
        if let Some((file, version)) = done.file()
            && !self.doc.document.is_current(file, version)
        {
            return;
        }
        match done {
            Done::Search {
                file, id, count, ..
            } => {
                let marked = self
                    .doc
                    .document
                    .search
                    .as_ref()
                    .is_some_and(|m| m.file == file);
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
                blocks_version,
                matches,
                ..
            } => self.found(file, id, (diff.as_ref(), &hidden, blocks_version), matches),
            Done::Diff {
                file,
                result,
                placed,
                ..
            } => self.loaded(file, result, placed),
            Done::Placed {
                file, diff, placed, ..
            } => self.placed_on(file, &diff, placed),
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
                self.doc
                    .document
                    .set_emphasis(file, &diff, hunk, emphasis, syntax);
                self.dirty |= self.visible.contains(&file);
            }
            Done::GapSyntax {
                file,
                diff,
                gap,
                spans,
                ..
            } => {
                self.doc.document.set_gap_syntax(file, &diff, gap, spans);
                self.dirty |= self.visible.contains(&file);
            }
            // The definitions are in the file's syntax slot already.
            Done::Scopes { file, .. } => self.dirty |= self.visible.contains(&file),
            Done::Structural {
                file, diff, result, ..
            } => {
                if self.doc.document.set_structural(file, &diff, result) {
                    self.doc.document.evict(CACHE_BYTES, &self.visible);
                    if self.doc.document.structural {
                        self.doc.normalize();
                        self.dirty = true;
                    }
                } else {
                    self.dirty |= self.doc.document.structural && self.visible.contains(&file);
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
                    if self.doc.document.is_current(file, version) {
                        self.doc.document.set_counts(file, counts);
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
        result: Result<Box<File>, Option<String>>,
    ) {
        let result = match result {
            Ok(loaded) => Ok(loaded),
            Err(Some(reason)) => Err(reason),
            Err(None) => return,
        };
        let failed = result.as_ref().err().cloned();
        if self.doc.set_rendered(file, diff, result) {
            self.doc.document.evict(CACHE_BYTES, &self.visible);
            if let Some(reason) = failed.filter(|_| self.doc.document.rendered()) {
                self.message(format!("{reason}; showing the lines"));
            }
            self.dirty = true;
        }
    }

    /// The count pass to start, once per list and only while the tree
    /// shows, not the comments: the files on screen in the tree first, then those near them,
    /// then the rest in order. Started again when the tree scrolls to files
    /// not counted yet.
    pub(crate) fn take_count_pass(&mut self) -> Option<Vec<Counted>> {
        if !self.tree_shown || self.enumerating || self.panel.is_some() {
            return None;
        }
        let files = self.doc.document.files();
        let scrolled = self
            .count_scroll
            .is_some_and(|row| row != self.tree.scroll())
            && self
                .tree
                .files_on_screen()
                .any(|f| files.get(f).is_some_and(view::Entry::wants_counts));
        if !self.count_pass && !scrolled {
            return None;
        }
        self.count_pass = false;
        self.count_scroll = Some(self.tree.scroll());
        let mut first = self.tree.files_near_screen();
        let mut wanted: Vec<bool> = files.iter().map(view::Entry::wants_counts).collect();
        first.retain(|&f| wanted.get_mut(f).is_some_and(std::mem::take));
        let rest = (0..files.len()).filter(|&f| wanted[f]);
        Some(
            first
                .iter()
                .copied()
                .chain(rest)
                .map(|f| (f, files[f].version(), files[f].change().clone()))
                .collect(),
        )
    }

    /// The languages of listed files not handed out yet, whose queries the
    /// pool compiles ahead of their first file being drawn. Nothing while
    /// highlighting is off; those listed meanwhile come once it's on.
    pub(crate) fn take_languages(&mut self) -> Vec<Language> {
        if !(self.highlight && self.color) {
            return Vec::new();
        }
        let new = self.languages[self.prepared..].to_vec();
        self.prepared = self.languages.len();
        new
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
        self.tree.rebuild(self.doc.document.files());
        if let Some(comments) = &mut self.comments {
            comments.locate(self.doc.document.files());
        }
        if !self.doc.document.files().is_empty() {
            self.tree.follow(self.doc.document.pos.file);
        }
    }

    /// Gives the tree the keys, its selection starting on the file at the
    /// top of the diff.
    fn focus_tree(&mut self) {
        if !self.tree_shown {
            self.show_tree(true);
        }
        self.pane = Pane::Tree;
        if !self.doc.document.files().is_empty() {
            self.tree.follow(self.doc.document.pos.file);
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

    /// The input after a question in the status bar: `y` does what it
    /// asked about, and anything else keeps things as they are.
    fn answer(&mut self, input: &Input, action: Action, declined: &str) -> Option<Action> {
        if *input == Input::Key(Key::Char('y'), Mods::NONE) {
            return Some(action);
        }
        self.message(declined);
        None
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the layers input passes through, in the order they take it"
    )]
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
        // Legacy ctrl+j is the line feed, which some terminals send for
        // enter; only the text box takes it as a key of its own.
        let input = match input {
            Input::Key(Key::Char('j'), Mods::CTRL) if self.doc.writing().is_none() => {
                Input::Key(Key::Enter, Mods::NONE)
            }
            input => input,
        };
        self.dirty = true;
        self.focus = None;
        self.unseen = None;
        if std::mem::take(&mut self.tip) {
            self.message = None;
        }
        if let Some((_, action, declined)) = self.confirm.take() {
            return self.answer(&input, action, declined);
        }
        if self.doc.writing().is_some() {
            return self.writing_input(input);
        }
        if self.menu.is_some() {
            return self.menu_input(&input);
        }
        if let ControlFlow::Break(done) = self.review_input(&input) {
            return done;
        }
        if self.commands.is_some() {
            return self.commands_input(&input);
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
            return self.picker_input(&input);
        }
        let pending = self.doc.document.pending_land.take();
        // Armed for the key right after `d` alone: a click or a paste
        // between them disarms it, as the click may focus another thread.
        let deleting = std::mem::take(&mut self.deleting);
        let (key, mods) = match input {
            Input::Key(key, mods) => (key, mods),
            Input::Paste(text) => {
                self.paste_line(&text);
                return None;
            }
            input => return self.pointer(&input),
        };
        if self.tree.editing && !mods.contains(Mods::CTRL) {
            self.filter_key(key);
            return None;
        }
        if self.search.as_ref().is_some_and(|s| s.editing) {
            self.search_key(key, mods);
            return None;
        }
        if let ControlFlow::Break(done) = self.panel_key(key, mods, deleting) {
            return done;
        }
        // A thread scrolled off screen loses the focus: its keys would act
        // on what can't be seen.
        if self.doc.focused().is_some() && !self.doc.focused_shown() {
            self.doc.set_focus(None);
        }
        if self.pane == Pane::Diff
            && self.doc.selection.is_none()
            && self.doc.focused().is_some()
            && let Ok(done) = self.thread_key(key, mods, deleting)
        {
            return done;
        }
        // A conflict's keys come after a thread's; a search going on keeps
        // `n` and `N` for its matches.
        let stepping = |a: &Act| matches!(a, Act::NextConflict | Act::PrevConflict);
        let conflict = (self.pane == Pane::Diff && self.has_conflicts())
            .then(|| self.keys.conflict_action(key, mods))
            .flatten()
            .filter(|a| self.search.is_none() || !stepping(a));
        if let Some(action) = conflict
            && let ControlFlow::Break(done) = self.conflict_act(action)
        {
            return done;
        }
        let mut action = self.keys.action(self.pane == Pane::Tree, key, mods)?;
        // `esc` backs out one layer at a time: overlays (above), then the
        // search or the tree's filter, the selection, the focused thread,
        // the tree's focus; with nothing left, it says how to quit, or
        // does what it's bound to (`keybind = esc=quit`).
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
            && pending.is_some_and(|(at, _)| at == self.doc.document.pos)
        {
            self.doc.document.pending_land = pending;
            self.message("waiting for this file's diff");
            return None;
        }
        // Armed by this key's last press, for `act` to find.
        if action == Act::DeleteDraft {
            self.deleting = deleting;
        }
        self.dispatch(action)
    }

    /// Runs `action` as its key does in the pane with the keys: lines
    /// selected in the diff take it first. Text selected in a comment
    /// stays only for `y`.
    fn dispatch(&mut self, action: Act) -> Option<Action> {
        if self.doc.comment_selection.is_some() && action != Act::Copy {
            self.doc.comment_selection = None;
            // `esc` backs out of it alone.
            if action == Act::Cancel && self.pane == Pane::Diff {
                return None;
            }
        }
        if self.doc.selection.is_some() && self.pane == Pane::Diff {
            match self.selecting(action) {
                Ok(done) => return done,
                Err(()) => self.doc.selection = None,
            }
        }
        self.act(action)
    }

    /// A paste with no overlay open: its first line goes into the search
    /// or the left panel's filter being typed. Anywhere else it's dropped, never
    /// read as keys.
    fn paste_line(&mut self, text: &str) {
        let line = text.lines().next().unwrap_or_default();
        if self.tree.editing {
            line.chars().for_each(|c| self.filter_key(Key::Char(c)));
        } else if self.search.as_ref().is_some_and(|s| s.editing) {
            line.chars()
                .for_each(|c| self.search_key(Key::Char(c), Mods::NONE));
        } else if self.panel.as_ref().is_some_and(|p| p.editing) {
            line.chars().for_each(|c| {
                drop(self.panel_key(Key::Char(c), Mods::NONE, false));
            });
        }
    }

    /// The first key bound to `action` in the tree (`tree`) or the diff,
    /// skipping keys the tree's own table takes for something else.
    fn key_for(&self, tree: bool, action: Act) -> Option<String> {
        self.keys
            .chords(true, action)
            .filter(|_| tree)
            .chain(self.keys.chords(false, action))
            .find(|c| self.keys.action(tree, c.key, c.mods) == Some(action))
            .map(crate::keys::Chord::name)
    }

    /// Input while the picker is open. Letters go to the query, `j` and `k`
    /// included; arrows, `ctrl+n`/`ctrl+p`, the wheel and taps move or pick.
    fn picker_input(&mut self, input: &Input) -> Option<Action> {
        let top = self.picker_top();
        let files = self.doc.document.files();
        let picker = self.picker.as_mut()?;
        // The rows on screen are what the keys act on; files that arrived
        // since go in first so a narrowed query sees them.
        picker.catch_up(files);
        let ctrl = |mods: Mods| mods.contains(Mods::CTRL);
        let pick = match *input {
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
            Input::Paste(ref text) => {
                let line = text.lines().next().unwrap_or_default();
                for c in line
                    .chars()
                    .filter(|c| c.is_alphanumeric() || "._-/".contains(*c))
                {
                    picker.push(files, c);
                }
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
            Act::CopyPrompt => {
                let done = self.copy_prompt();
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
            // The panel's copy acts on the selection, so opening it keeps it;
            // a comment goes on it, and staging takes its lines.
            Act::Comment
            | Act::AskAgent
            | Act::FixWithAgent
            | Act::Stage
            | Act::Unstage
            | Act::Discard
            | Act::SuggestChange
            | Act::CommentInEditor
            | Act::ScrollLeft
            | Act::ScrollRight
            | Act::ScrollReset
            | Act::CopyPath
            | Act::CopyAbsolutePath
            | Act::OpenTerminal
            | Act::Help
            | Act::Leader
            | Act::Quit => return Ok(self.act(action)),
            _ => return Err(()),
        }
        Ok(None)
    }

    /// Runs `action` for the pane with the keys.
    #[expect(
        clippy::too_many_lines,
        reason = "a dispatch: an arm per action or group of them"
    )]
    fn act(&mut self, action: Act) -> Option<Action> {
        let height = isize::try_from(self.doc.height.max(1)).unwrap_or(isize::MAX);
        let half = (height / 2).max(1);
        let tree = self.pane == Pane::Tree;
        match action {
            // Nothing else to back out of: the asks running stop.
            Act::Cancel if !tree && self.asking.busy() => {
                let effects = self.asking.stop(None);
                return self.review_effects(effects);
            }
            Act::AskAgent => return self.ask_agent(),
            Act::FixWithAgent => return self.fix_with_agent(),
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
                    self.leave_panel();
                } else {
                    self.focus_tree();
                }
            }
            Act::ToggleTree => self.toggle_tree(),
            Act::OpenPicker => {
                self.doc.selection = None;
                self.picker = Some(Picker::new(self.doc.document.files()));
            }
            Act::FilterTree => {
                self.close_panel();
                self.focus_tree();
                self.tree.editing = true;
            }
            Act::Search => self.start_search(),
            Act::ParentFolder => {
                // From the diff or the comments, the tree starts on the file
                // shown; in the tree, each `u` climbs a folder further.
                if !tree || self.close_panel() {
                    self.focus_tree();
                }
                self.tree.select_parent();
            }
            Act::NextChange | Act::PrevChange => {
                let before = self.doc.document.pos.file;
                self.change(action == Act::NextChange);
                // On past files the tree's filter hides.
                if self.doc.document.pos.file != before
                    && !self.tree.in_scope(self.doc.document.pos.file)
                {
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
            Act::OpenTerminal => return Some(Action::OpenTerminal),
            Act::Stage => return self.edit_lines(Edit::Stage),
            Act::Unstage => return self.edit_lines(Edit::Unstage),
            Act::Discard => return self.edit_lines(Edit::Discard),
            Act::NextConflict
            | Act::PrevConflict
            | Act::KeepCurrent
            | Act::TakeIncoming
            | Act::KeepBoth
            | Act::TakeBase
            | Act::WriteResolution => {
                // Bound in the main table: no change to go on to instead.
                if let ControlFlow::Break(done) = self.conflict_act(action) {
                    return done;
                }
                self.message("no conflict that way");
            }
            Act::Comment
            | Act::SuggestChange
            | Act::CommentFile
            | Act::Comments
            | Act::NextThread
            | Act::PrevThread
            | Act::ToggleThread
            | Act::ToggleThreads
            | Act::MoveThread
            | Act::CommentInEditor
            | Act::CommentedFiles => return self.comment_action(action),
            // Bound outside the thread's table: on the thread there is.
            Act::Reply
            | Act::EditDraft
            | Act::DeleteDraft
            | Act::Resolve
            | Act::Adopt
            | Act::ApplySuggestion
            | Act::OpenOnHost => {
                let deleting = std::mem::take(&mut self.deleting);
                match self.thread_act(action, deleting) {
                    Ok(done) => return done,
                    Err(()) if self.target_thread().is_none() => {
                        self.message("no thread here; m goes to the next");
                    }
                    Err(()) => {}
                }
            }
            Act::ReviewPull => return self.update_review(),
            Act::ReviewPost => return self.open_post(),
            Act::OpenReview => return self.open_review_picker(),
            Act::SinceLastReview => return self.toggle_since_review(),
            Act::WholeReview => return self.toggle_whole_review(),
            Act::Help => self.help = Some(Help::default()),
            Act::ScrollRight | Act::ScrollLeft | Act::ScrollReset => self.scroll_code(action),
            Act::Select => self.start_selection(),
            Act::Copy if self.doc.comment_selection.is_some() => {
                let done = self.copy();
                if done.is_some() {
                    self.doc.comment_selection = None;
                }
                return done;
            }
            Act::Copy if self.doc.selection.is_some() => return self.copy(),
            Act::Copy => self.message("nothing selected; v selects lines"),
            Act::CopyPath => return self.copy_path(),
            Act::CopyAbsolutePath => return self.copy_full_path(),
            Act::CopyComment | Act::CopyThread => {
                return self.copy_thread(action == Act::CopyThread);
            }
            Act::CopyPrompt => return self.copy_prompt(),
            Act::HalfPageDown => self.scroll_diff(half),
            Act::HalfPageUp => self.scroll_diff(-half),
        }
        None
    }

    fn scroll_diff(&mut self, rows: isize) {
        self.doc.scroll(rows);
        self.scrolled = true;
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
                self.doc.set_structural(!self.doc.document.structural);
                self.remember("syntax-diff", self.doc.document.structural.to_string());
                if self.doc.document.structural && self.doc.document.is_partial() {
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
        let on = !self.doc.document.rendered();
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
        let entry = self.doc.document.files().get(self.doc.document.pos.file)?;
        if matches!(entry.source(), Source::Piped(_)) {
            return Some("a piped diff has only its hunks; nothing to render".to_owned());
        }
        let path = entry.change().path.clone();
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
        let found = plugin::find(plugin::PREFIX, name);
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
            self.kept = false;
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
        let document = &self.doc.document;
        let pos = document.pos;
        if !document.rendered() || !document.missing(pos, pos).any(|m| m == Missing::Render) {
            return None;
        }
        let file = pos.file;
        let name = self
            .renderers
            .for_path(&document.files()[file].change().path)?
            .to_owned();
        let diff = Arc::clone(document.diff_of(file)?);
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
        if !self.doc.document.rendered() || self.renderers.is_empty() {
            return None;
        }
        let entry = self.doc.document.files().get(self.doc.document.pos.file)?;
        let name = self.renderers.for_path(&entry.change().path)?;
        match entry.render() {
            Some(Render::Failed(reason)) => Some(format!("lines: {reason}")),
            None if self
                .render
                .as_ref()
                .is_some_and(|t| t.file == self.doc.document.pos.file) =>
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
        let start = self.doc.document.pos;
        loop {
            let before = self.doc.document.pos.file;
            if forward {
                self.doc.next_file();
            } else {
                self.doc.prev_file();
            }
            let now = self.doc.document.pos.file;
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
            self.doc.set_highlight(self.highlight);
            if let Some(comments) = &mut self.comments {
                comments.set_highlight(self.highlight);
            }
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
        self.doc.document.restale();
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
        if self.doc.document.is_partial() || (full && self.piped) {
            self.message("a piped diff has only its hunks; the lines between them aren't in it");
        } else if full {
            let before = self.doc.document.full;
            self.doc.toggle_full();
            if self.doc.document.full != before {
                let context = if self.doc.document.full {
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
            Act::ScrollDown => self.scroll_diff(1),
            Act::ScrollUp => self.scroll_diff(-1),
            Act::PageDown => self.scroll_diff(height),
            Act::PageUp => self.scroll_diff(-height),
            Act::Top => self.doc.document.top(),
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
        let from = self.doc.document.pos.file;
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
        let commit = self.doc.document.commit_of(file);
        if commit != self.doc.document.commit_of(from)
            && let Some(c) = commit
        {
            let document = &self.doc.document;
            let label = escape(&document.files()[document.commits()[c]].change().path);
            let total = self.doc.document.commits().len();
            self.message(format!("{arrow} commit {}/{total}: {label}", c + 1));
            return;
        }
        let path = escape(&self.doc.document.files()[file].change().path);
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
        let tapped = self.take_tapped();
        self.doc.set_focus(None);
        self.doc.comment_selection = None;
        if !self.doc.select(tapped) {
            self.message("no lines of code here to select");
        }
    }

    /// The row last tapped, if it's still on screen; used once.
    fn take_tapped(&mut self) -> Option<(Pos, Side)> {
        self.tapped
            .take()
            .filter(|(pos, _)| (0..self.doc.height).any(|y| self.doc.row_at(y) == Some(*pos)))
    }

    /// `y`: the selected lines on the clipboard, or the Markdown of the
    /// text selected in a comment.
    fn copy(&mut self) -> Option<Action> {
        if self.doc.comment_selection.is_some() {
            let (text, whole) = self.doc.comment_selected()?;
            if text.is_empty() {
                return None;
            }
            let note = if whole {
                "copied a comment".to_owned()
            } else {
                format!(
                    "copied {} of a comment",
                    session::plural(text.chars().count(), "char")
                )
            };
            return self.copy_text(text, note);
        }
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
        let mut note = format!("copied {}", session::plural(n, unit));
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

    /// `text` on the clipboard, saying `note`, unless it's more than a copy
    /// can hold.
    fn copy_text(&mut self, text: String, note: String) -> Option<Action> {
        if text.len() > term::MAX_CLIPBOARD_BYTES {
            self.message(format!(
                "too much to copy: {}KB, over the {}KB a copy can hold",
                text.len() / 1000,
                term::MAX_CLIPBOARD_BYTES / 1000
            ));
            return None;
        }
        Some(Action::Copy {
            text: text.into_bytes(),
            note,
        })
    }

    /// `copy_comment`: the focused thread's comment last clicked, else its
    /// first, as written; `copy_thread` (`whole`): the thread, each comment
    /// under its author and date.
    fn copy_thread(&mut self, whole: bool) -> Option<Action> {
        let Some(thread) = self.target_thread() else {
            self.message("no thread focused to copy; m goes to the next");
            return None;
        };
        if whole {
            let note = format!(
                "copied a thread of {}",
                session::plural(thread.comments.len(), "comment")
            );
            return self.copy_text(model::text::thread_text(&thread), note);
        }
        let clicked = self
            .tapped_comment
            .as_ref()
            .filter(|(root, _)| *root == thread.root)
            .and_then(|(_, id)| thread.comments.iter().find(|n| n.id == *id));
        let text = clicked
            .or_else(|| thread.comments.first())?
            .text
            .trim_end_matches(['\n', '\r']);
        self.copy_text(text.to_owned(), "copied a comment".to_owned())
    }

    /// `copy_prompt`: the focused thread, else the selected lines, as a
    /// prompt for a coding agent.
    fn copy_prompt(&mut self) -> Option<Action> {
        let made = if let Some(thread) = self.target_thread() {
            (self.thread_prompt(&thread.root, &thread.at))
                .map(|made| made.map(|(made, what)| (made, what.to_owned())))
        } else if self.doc.selection.is_some() {
            self.doc
                .comment_target(None)
                .and_then(|(file, side, lines)| {
                    let n = lines.len();
                    let loaded = self.doc.document.file(file)?;
                    let made = prompt::selection(model::line_diff(loaded), side, lines);
                    Some(Ok((made, session::plural(n, "line"))))
                })
        } else {
            Some(Err(
                "nothing to copy as a prompt: focus a thread, or v selects lines",
            ))
        };
        match made? {
            Ok((made, what)) => {
                let mut note = format!("copied {what} as a prompt");
                if made.trimmed {
                    let _ = write!(note, ", cut to fit {}KB", prompt::MAX_BYTES >> 10);
                }
                self.copy_text(made.text, note)
            }
            Err(why) => {
                self.message(why);
                None
            }
        }
    }

    /// The thread with first comment `root`, shown `at`, as a prompt, on
    /// its file's diff when it's loaded.
    fn thread_prompt(
        &self,
        root: &str,
        at: &model::At,
    ) -> Option<Result<(prompt::Prompt, &'static str), &'static str>> {
        let comments = self.comments.as_ref()?;
        let list = comments.thread(root);
        if list.is_empty() {
            return Some(Err(
                "a thread on another branch can't be made a prompt; move it here first",
            ));
        }
        let diff = (comments.home_of(root))
            .and_then(|path| comments.file_of(path.as_bytes()))
            .and_then(|file| self.doc.document.file(file))
            .map(model::line_diff);
        let made = prompt::thread(&list, at, diff.map(|d| &**d));
        Some(Ok((made, "a thread")))
    }

    /// What came of `open_terminal`: where no tab opened at `dir`, a `cd`
    /// there goes on the clipboard instead.
    fn terminal_opened(&mut self, dir: &Path, opened: Result<String, String>) -> Option<Action> {
        match opened {
            Ok(note) => {
                self.message(note);
                None
            }
            Err(why) => {
                let cd = crate::terminal::cd(dir);
                Some(Action::Copy {
                    note: format!("{why}; copied {}", escape(&cd)),
                    text: cd,
                })
            }
        }
    }

    /// `Y`: the path of the file shown, as the repository has it.
    fn copy_path(&mut self) -> Option<Action> {
        let entry = self.doc.document.files().get(self.doc.document.pos.file)?;
        let path = match &entry.source() {
            Source::Commit(commit) => commit.id.as_ref()?.as_bytes().into(),
            Source::Page { .. } => {
                self.message("the conversation has no path");
                return None;
            }
            _ => entry.change().path.clone(),
        };
        Some(Action::Copy {
            note: format!("copied {}", escape(&path)),
            text: path.into(),
        })
    }

    /// Where the file shown is on disk, which the loop knows.
    fn copy_full_path(&mut self) -> Option<Action> {
        let entry = self.doc.document.files().get(self.doc.document.pos.file)?;
        if entry.is_commit() {
            self.message(if entry.is_page() {
                "the conversation has no file on disk"
            } else {
                "a commit has no file on disk"
            });
            return None;
        }
        Some(Action::CopyFullPath(entry.change().path.clone()))
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
                    let viewed = self.doc.document.files()[file].viewed();
                    self.doc.document.set_viewed(file, !viewed);
                }
                None
            }
            Act::Cancel if !self.tree.filter().is_empty() || self.tree.commented() => {
                self.tree.set_commented(None);
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
        self.leave_panel();
    }

    /// Gives the diff the keys; a left panel over it closes.
    fn leave_panel(&mut self) {
        self.pane = Pane::Diff;
        if !self.side_tree() {
            self.show_tree(false);
        }
    }

    /// `e`: the file in the editor at the line last tapped, as `v` starts
    /// there, else at the top line.
    fn edit(&mut self) -> Option<Action> {
        let tapped = self.take_tapped().map(|(pos, _)| pos);
        let file = tapped.map_or(self.doc.document.pos.file, |pos| pos.file);
        let entry = self.doc.document.files().get(file)?;
        if entry.is_commit() {
            self.message(if entry.is_page() {
                "the conversation has no file to open"
            } else {
                "a commit has no file to open"
            });
            return None;
        }
        let change = &entry.change();
        if change.new.is_none() {
            self.message("this file is deleted; there's nothing to open");
            return None;
        }
        let path = change.path.clone();
        let line = match tapped {
            Some(pos) => self.doc.line_at(pos),
            None => self.doc.line_at_top(),
        };
        Some(Action::Edit { path, line })
    }

    /// The search's scan of the file shown, once it's diffed (a page has
    /// only its comments) and until its matches are in for the lines it
    /// shows now.
    fn find_job(&self) -> Option<Job> {
        let search = self.search.as_ref().filter(|s| !s.needle.text.is_empty())?;
        let file = self.doc.document.pos.file;
        let document = &self.doc.document;
        let (diff, hidden) = if document.files().get(file)?.is_page() {
            (None, Vec::new())
        } else {
            let diff = Arc::clone(document.diff_of(file)?);
            (Some(diff), document.hidden_lines(file)?)
        };
        let blocks_version = document.blocks_version(file);
        if (document.search.as_ref()).is_some_and(|m| {
            m.file == file && m.hidden == hidden && m.blocks_version == blocks_version
        }) {
            return None;
        }
        Some(Job::Find {
            file,
            version: self.version(file),
            diff,
            hidden,
            blocks: document.search_blocks(file),
            blocks_version,
            needle: Arc::clone(&search.needle),
            id: search.id,
            live: Arc::clone(&self.live_search),
        })
    }

    /// Work for the pool, most wanted first: diffs of files on screen,
    /// comments placed again on files loaded, the search's scan of the
    /// file shown, emphasis for hunks on screen,
    /// highlighting of revealed lines on screen, the definitions of the
    /// file shown once it's highlighted, syntax diffs of files on
    /// screen when that view is on, diffs of nearby files, then the
    /// search's counts of the other files.
    pub(crate) fn jobs(&self) -> Vec<Job> {
        let files = self.doc.document.files();
        let highlight = self.comments.as_ref().is_some_and(Comments::highlight);
        // Only a file on screen leaves its lost code to a placement after.
        let diff = |file: usize| {
            files[file].wants_diff().then(|| Job::Diff {
                file,
                version: files[file].version(),
                change: files[file].change().clone(),
                piped: match &files[file].source() {
                    Source::Piped(piped) => Some(Arc::clone(piped)),
                    Source::Repo | Source::Commit(_) | Source::Page { .. } => None,
                },
                choices: files[file].choices().cloned(),
                paint: highlight && !self.visible.contains(&file),
            })
        };
        let mut jobs: Vec<Job> = self.visible.clone().filter_map(diff).collect();
        // Comments placed again after they changed, on a file loaded.
        let pending = (self.comments.as_ref()).map_or_else(Vec::new, |c| c.pending(files));
        jobs.extend(pending.iter().filter_map(|&file| {
            let comments = self.comments.as_ref()?;
            Some(Job::Place(PlaceJob {
                file,
                version: files[file].version(),
                annotated: comments.annotated(&files[file].change().path)?,
                diff: Arc::clone(model::line_diff(files[file].file()?)),
                paint: highlight,
            }))
        }));
        jobs.extend(self.find_job());
        let mut structural = None;
        for (file, missing) in self.doc.missing() {
            let Some(loaded) = files.get(file).and_then(|e| e.file()) else {
                continue;
            };
            let version = files[file].version();
            let diff = Arc::clone(loaded.diff());
            match missing {
                Missing::Emphasis(hunk) => jobs.push(Job::Emphasis {
                    file,
                    version,
                    hunk,
                    diff,
                    syntax: loaded.syntax().cloned(),
                }),
                Missing::GapSyntax(gap) => {
                    jobs.extend(loaded.syntax().map(|syntax| Job::GapSyntax {
                        file,
                        version,
                        gap,
                        lines: loaded.gap_lines(gap),
                        diff,
                        syntax: Arc::clone(syntax),
                    }));
                }
                Missing::Scopes => {
                    jobs.extend(loaded.syntax().map(|syntax| Job::Scopes {
                        file,
                        version,
                        diff,
                        syntax: Arc::clone(syntax),
                    }));
                }
                Missing::Structural => {
                    structural = Some(Job::Structural {
                        file,
                        version,
                        diff,
                    });
                }
                // The render target, settled before the frame, asks for it.
                Missing::Render => {}
            }
        }
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
        jobs.extend(structural);
        let ahead = self.visible.end..(self.visible.end + PREFETCH_AHEAD).min(files.len());
        let behind = self.visible.start.saturating_sub(PREFETCH_BEHIND)..self.visible.start;
        let near = behind.start..ahead.end;
        jobs.extend(ahead.chain(behind).filter_map(diff));
        // Diffs of files with comments, to place them: the panel lists
        // those not found on a line.
        jobs.extend(
            pending
                .into_iter()
                .filter(|f| !near.contains(f))
                .filter_map(diff),
        );
        jobs.extend(self.search_jobs());
        jobs
    }
}

/// How `hint` reads with the key that does it here, as `p to update`;
/// `None` when no key does.
pub(super) fn hint_words(keys: &Keys, hint: Hint) -> Option<String> {
    let key = |action| {
        keys.chords(false, action)
            .next()
            .map(crate::keys::Chord::name)
    };
    Some(match hint {
        Hint::Update => format!("{} to update", key(Act::ReviewPull)?),
        Hint::Submit => format!("{} submits", key(Act::ReviewPost)?),
        Hint::OpenPosted => format!("{} opens it", review::OPEN_POSTED),
        Hint::GoToUnposted => format!("{} goes to them", review::GO_TO_UNPOSTED),
        Hint::StepThreads => format!(
            "{}/{} go between threads",
            key(Act::NextThread)?,
            key(Act::PrevThread)?
        ),
        Hint::StopPosting => format!("{} again stops it", key(Act::Quit)?),
        Hint::OtherBranches => format!("{} lists them", comments::OTHER_BRANCHES),
        Hint::Comment => format!("{} comments on the line at the top", key(Act::Comment)?),
    })
}

#[cfg(test)]
mod tests;

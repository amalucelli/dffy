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

mod review;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::ops::{ControlFlow, Range};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dffy_core::bstr::BString;
use dffy_core::diff::Whitespace;
use dffy_core::document::{
    File, Found, Match, Matches, Missing, Needle, Pos, Render, Side, Source, State,
};
use dffy_core::git::{FileChange, FileDiff, Revisions, Status, Update};
use dffy_core::highlight::Language;
use dffy_core::view::Attributes;
use dffy_core::watch::Notice;
use unicode_width::UnicodeWidthStr;

use super::chrome::{self, Chrome, Cut, Heading, Switcher, Title};
use super::commands::{self, Commands, Outcome};
use super::comments::{self, Comments, Compose, Panel, Writes, Writing};
use super::help::Help;
use super::menu::{self, Menu, Run};
use super::picker::{self, Picker};
use super::piped;
use super::textbox::TextBox;
use super::tree::{self, Item, Totals, Tree};
use super::view::{Block, Doc, Hit};
use super::worker::{Counted, Done, Job, RenderJob};
use super::{Event, view};
use crate::config::{
    self, Background, Config, Context, DefaultDiff, Indicators, ModifiedLines, StatusBar, TreeMode,
    View,
};
use crate::daemon::client;
use crate::keys::{Action as Act, Keys};
use crate::plugin::review::{Naming, Said};
use crate::plugin::{self, Program, Renderers};
use crate::print;
use crate::render::{Layout, escape, thousands};
use crate::review::{Change, Op, Target};
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
/// Unchanged lines revealed above and below a thread's lines that a gap
/// hid.
const THREAD_CONTEXT: usize = 2;
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
    /// Run `dffy-review-<plugin> <args>` in the background, with
    /// `--summary-file` holding `summary` when there is one; what it says
    /// comes back as [`Event::Review`] under `id`. A `prepare`, `pull` or
    /// `post` stops the one before it.
    Plugin {
        id: u64,
        plugin: String,
        args: Vec<String>,
        comments: Option<PathBuf>,
        summary: Option<String>,
    },
    /// List `revisions` from now on, when given, reading `comments` when
    /// it's another file. `moved` is the review's head before and after, to
    /// count commits.
    Retarget {
        revisions: Option<Revisions>,
        comments: Option<PathBuf>,
        moved: Option<(String, String)>,
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

/// The thread to focus in a file once its threads are placed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seek {
    /// Its first (`m`), or with `true` its last (`M`).
    Edge(bool),
    /// The one with this root, selected in the comments panel.
    Root(String),
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
    tapped_note: Option<(String, String)>,
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
    /// `d` was pressed once on the focused thread; another deletes.
    deleting: bool,
    /// Replying to an outdated thread said once the plugin's warning.
    warned_outdated: bool,
    /// A file gone to for a thread before its threads were placed, and
    /// which of them to focus once they are.
    thread_pending: Option<(usize, Seek)>,
    /// The review plugin's name, from the config or `--review`.
    review: Option<String>,
    /// The review plugin's words, when it describes itself.
    naming: Option<Naming>,
    /// The review shown, in review mode.
    reviewing: Option<review::Mode>,
    /// The review plugin's `prepare`, `pull` or post, while it runs.
    job: Option<review::Running>,
    /// The last run id handed out.
    run_id: u64,
    /// The reviews to pick from.
    reviews: Option<review::Picker>,
    /// The dialog that posts the review.
    posting: Option<review::Dialog>,
    /// Threads an update brought, by root, in list order: `m` goes to
    /// them first.
    new_threads: Vec<String>,
    /// A quit was asked for while a post runs; the next one stops it.
    quit_armed: bool,
    /// When the last key or click came: the checks stop after a while
    /// without one.
    last_input: Instant,
    /// The current branch, which new local comments go on; `None` on a
    /// detached HEAD.
    branch: Option<String>,
    /// The review just posted, which `o` opens until another key.
    link: Option<String>,
    /// The `list` run for the review picker, beside any other run.
    listing: Option<u64>,
    /// The `detect` run looking for the branch's review at startup.
    detecting: Option<u64>,
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
            tapped_note: None,
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
            deleting: false,
            warned_outdated: false,
            thread_pending: None,
            review: config.review.clone(),
            naming: None,
            reviewing: None,
            job: None,
            run_id: 0,
            reviews: None,
            posting: None,
            new_threads: Vec::new(),
            quit_armed: false,
            last_input: Instant::now(),
            branch: None,
            link: None,
            listing: None,
            detecting: None,
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
            others: None,
            piped: false,
            watch: Watch::Off,
            focus: None,
            frame: 0,
            visible: 0..1,
            dirty: true,
            kept: false,
            scrolled: false,
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

    pub(crate) fn set_naming(&mut self, naming: Option<Naming>) {
        self.naming = naming;
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
        self.doc.layout = layout;
        self.doc.height = self.screen.height().saturating_sub(2);
        self.tree.set_height(self.tree_rows().1);
        self.doc.normalize();
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

    pub(crate) fn message(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), Instant::now() + MESSAGE_TIME));
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
            .chain(self.check_deadline())
            .chain(self.plan_deadline())
            .min()
    }

    pub(crate) fn mouse(&self) -> bool {
        self.mouse
    }

    pub(crate) fn event(&mut self, event: Event) -> Option<Action> {
        let kept = std::mem::take(&mut self.kept);
        self.scrolled = false;
        if matches!(event, Event::Input(_)) {
            if self.last_input.elapsed() >= review::IDLE_AFTER {
                self.resume_checks();
            }
            self.last_input = Instant::now();
        }
        let mut action = self.handle(event);
        self.sync_threads();
        self.kept = kept && self.scrolled;
        // Stopping a post halfway leaves the review half sent until the
        // next post, so the first quit asks again.
        if !self.posting_now() {
            self.quit_armed = false;
        } else if action == Some(Action::Quit)
            && !std::mem::replace(&mut self.quit_armed, true)
            && let Some(note) = self.still_posting()
        {
            self.message(note);
            action = None;
        }
        action
    }

    /// The terminal went away or a signal says to stop: the text box's
    /// text is saved, as `ctrl+c` saves it.
    pub(crate) fn interrupted(&mut self) {
        self.close_box();
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
        self.doc.set_threads(file, threads);
    }

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
            Event::Comments => self.reload_comments(),
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
                return self.due_check().or_else(|| self.due_plan());
            }
            Event::Review { id, said } => return self.review_event(id, said),
            Event::ReviewMoved(count) => self.review_updated(count),
            // The loop handles these; they need the terminal.
            Event::Resize | Event::Interrupt => {}
        }
        None
    }

    fn review_event(&mut self, id: u64, said: Said) -> Option<Action> {
        self.before_import(id);
        // Plugins write the comments file as they go: a page of threads at
        // a time, refs once posted. Read before the handler, which may look
        // the threads up.
        if matches!(said, Said::Progress(_) | Said::Session(_) | Said::Done(_)) {
            self.reload_comments();
        }
        self.review_said(id, said)
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
                matches,
                ..
            } => self.found(file, id, &diff, &hidden, matches),
            Done::Diff { file, result, .. } => {
                self.doc.document.search.take_if(|m| m.file == file);
                if self.doc.load(file, result) {
                    self.place(file);
                    self.doc.document.evict(CACHE_BYTES, &self.visible);
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
        self.unseen = None;
        if std::mem::take(&mut self.tip) {
            self.message = None;
        }
        if self.doc.writing.is_some() {
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
        self.dispatch(action)
    }

    /// Runs `action` as its key does in the pane with the keys: lines
    /// selected in the diff take it first. Text selected in a comment
    /// stays only for `y`.
    fn dispatch(&mut self, action: Act) -> Option<Action> {
        if self.doc.note_selection.is_some() && action != Act::Copy {
            self.doc.note_selection = None;
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

    /// Input other than a key with no overlay open: the mouse.
    fn pointer(&mut self, input: &Input) -> Option<Action> {
        match *input {
            Input::Scroll { delta, x, .. } => {
                let rows = isize::from(delta) * WHEEL_ROWS;
                if !self.over_tree(usize::from(x)) {
                    self.scroll_diff(rows);
                } else if let Some(panel) = &mut self.panel {
                    if let Some(root) = panel.scroll_by(isize::from(delta)) {
                        self.follow_thread(&root);
                    }
                } else {
                    self.tree.scroll_by(rows);
                }
            }
            Input::Click { x, y } => self.click(usize::from(x), usize::from(y)),
            Input::Drag { x, y } => self.drag(usize::from(x), usize::from(y)),
            Input::Release { .. } => return self.release(),
            Input::RightClick { x, y } => self.open_menu(usize::from(x), usize::from(y)),
            Input::Key(..) | Input::Background { .. } | Input::Paste(_) => {}
        }
        None
    }

    /// A right click elsewhere opens the menu there instead.
    fn menu_input(&mut self, input: &Input) -> Option<Action> {
        // An armed `d` is kept for the key right after it, never a later one.
        self.deleting = false;
        let size = (self.screen.width(), self.screen.height());
        let menu = self.menu.as_mut()?;
        let outcome = match *input {
            Input::Key(key, mods) => menu.key(key, mods),
            Input::Click { x, y } => menu.tap((usize::from(x), usize::from(y)), size),
            Input::RightClick { x, y } => {
                self.menu = None;
                self.open_menu(usize::from(x), usize::from(y));
                return None;
            }
            _ => menu::Outcome::Stay,
        };
        match outcome {
            menu::Outcome::Stay => None,
            menu::Outcome::Close => {
                self.menu = None;
                None
            }
            menu::Outcome::Run(run) => {
                self.menu = None;
                match run {
                    Run::Action(action) => self.dispatch(action),
                    Run::CopyLine => {
                        self.dispatch(Act::Select);
                        self.dispatch(Act::Copy)
                    }
                    Run::Thread(c) => self
                        .thread_key(Key::Char(c), Mods::NONE, false)
                        .ok()
                        .flatten(),
                    Run::Reveal(separator) => {
                        self.doc.aim(separator);
                        self.dispatch(Act::Expand)
                    }
                }
            }
        }
    }

    /// A right click: aims at what's under it as a left click does, without
    /// opening, folding or revealing anything, then lists what the keys do
    /// to it there. With nothing to act on, no menu opens.
    fn open_menu(&mut self, x: usize, y: usize) {
        self.press = None;
        self.drag_edge = None;
        self.deleting = false;
        let (tree_top, tree_rows) = self.tree_rows();
        let entries = if self.over_tree(x) && (tree_top..tree_top + tree_rows).contains(&y) {
            // Under the panel's top border.
            y.checked_sub(tree_top + 1)
                .map(|line| self.aim_tree(line))
                .unwrap_or_default()
        } else if y == 0 || y + 1 >= self.screen.height() || x + 1 == self.screen.width() {
            Vec::new()
        } else {
            self.aim_diff(x, y)
        };
        if !entries.is_empty() {
            self.menu = Some(Menu::new((x, y), entries));
        }
    }

    /// Selects the tree's row at `line`, the keys on the tree; for a file,
    /// shows it, as moving onto it does, and lists the tree's keys for it.
    fn aim_tree(&mut self, line: usize) -> Vec<menu::Entry> {
        if let Some(panel) = &mut self.panel {
            let Some(root) = panel.click(line) else {
                return Vec::new();
            };
            self.pane = Pane::Tree;
            // Copy comment takes the first: no comment of the row was tapped.
            self.tapped_note = None;
            self.follow_thread(&root);
            let Some(thread) = self.target_thread() else {
                return Vec::new();
            };
            let shown = self.doc.focused().is_some_and(|t| t.root == root);
            return self.thread_entries(&thread, shown);
        }
        let Some(item) = self.tree.click(line) else {
            return Vec::new();
        };
        self.pane = Pane::Tree;
        let Item::File(file) = item else {
            return Vec::new();
        };
        // `e` would open a line tapped before, maybe in another file.
        self.tapped = None;
        self.doc.goto_file(file);
        let entry = &self.doc.document.files()[file];
        let on_disk = !entry.is_commit() && !self.piped;
        let editable = on_disk && entry.change().new.is_some();
        let mut entries = vec![self.menu_entry(true, "Copy path", Act::CopyPath)];
        if on_disk {
            entries.push(self.menu_entry(true, "Copy absolute path", Act::CopyAbsolutePath));
        }
        if editable {
            entries.push(self.menu_entry(true, "Open in editor", Act::Edit));
        }
        let mark = if entry.viewed() {
            "Mark unviewed"
        } else {
            "Mark viewed"
        };
        entries.push(self.menu_entry(true, mark, Act::MarkViewed));
        if self.comments.is_some() {
            if on_disk {
                entries.push(self.menu_entry(true, "Comment on file", Act::CommentFile));
            }
            let filter = if self.tree.commented() {
                "Show every file"
            } else {
                "Show only files with comments"
            };
            entries.push(self.menu_entry(true, filter, Act::CommentedFiles));
        }
        entries
    }

    /// Aims at the diff's screen cell `x`, `y`: a thread takes the focus, a
    /// separator is the one `x` reveals, and a line of code is where `v`
    /// starts, the selection kept only when it holds the line. Lists the
    /// keys for it.
    fn aim_diff(&mut self, x: usize, y: usize) -> Vec<menu::Entry> {
        self.pane = Pane::Diff;
        self.doc.note_selection = None;
        match self.doc.block_row_at(y - 1) {
            Some((Block::Thread(i), _)) => {
                let thread = self.doc.threads()[i].clone();
                self.doc.selection = None;
                self.doc.set_focus(Some(thread.root.clone()));
                self.tapped_note = self.doc.note_at(y - 1);
                return self.thread_entries(&thread, true);
            }
            Some((Block::New, _)) => return Vec::new(),
            None => self.doc.set_focus(None),
        }
        if let Some(separator) = self.doc.separator_at(y - 1) {
            self.doc.selection = None;
            return vec![menu::Entry {
                run: Run::Reveal(separator),
                ..self.menu_entry(false, "Reveal hidden lines", Act::Expand)
            }];
        }
        let Some((pos, side)) = self.code_at(x, y) else {
            self.doc.selection = None;
            return Vec::new();
        };
        let selected = self.doc.selection_holds(pos, side);
        if !selected {
            self.doc.selection = None;
        }
        self.tapped = Some((pos, side));
        let entry = &self.doc.document.files()[pos.file];
        let editable = !entry.is_commit() && !self.piped && entry.change().new.is_some();
        let commentable = self.comments.is_some() && !entry.is_commit();
        let mut entries = Vec::new();
        if commentable {
            let label = if selected {
                "Comment on selection"
            } else {
                "Comment on line"
            };
            entries.push(self.menu_entry(false, label, Act::Comment));
            entries.push(self.menu_entry(false, "Suggest change", Act::SuggestChange));
        }
        if selected {
            entries.push(self.menu_entry(false, "Copy selection", Act::Copy));
        } else {
            let key = match (
                self.key_for(false, Act::Select),
                self.key_for(false, Act::Copy),
            ) {
                (Some(v), Some(y)) => format!("{v} {y}"),
                _ => String::new(),
            };
            entries.push(menu::Entry {
                label: "Copy line".to_owned(),
                key,
                run: Run::CopyLine,
            });
        }
        if editable {
            entries.push(self.menu_entry(false, "Open in editor", Act::Edit));
        }
        entries
    }

    /// The thread keys, as far as they'd act on `thread`, then its copies;
    /// folding only when the diff shows it (`folds`).
    fn thread_entries(&self, thread: &comments::Thread, folds: bool) -> Vec<menu::Entry> {
        let key = |label: &str, c: char| menu::Entry {
            label: label.to_owned(),
            key: c.to_string(),
            run: Run::Thread(c),
        };
        let copies = [
            self.menu_entry(false, "Copy comment", Act::CopyComment),
            self.menu_entry(false, "Copy thread", Act::CopyThread),
        ];
        // The conversation is read-only, and never folds.
        if matches!(thread.at, comments::At::Conversation { .. }) {
            return (thread.url.as_deref().is_some_and(is_web))
                .then(|| key("Open on host", 'o'))
                .into_iter()
                .chain(copies)
                .collect();
        }
        let mut entries = vec![key("Reply", 'r')];
        if thread.notes.iter().any(|n| !n.posted()) {
            entries.push(key("Edit draft", 'e'));
            entries.push(key("Delete draft", 'd'));
        }
        if thread.unposted() {
            entries.push(key("Make it yours", 'a'));
        }
        if thread.branch.is_some() {
            entries.push(key("Move to this branch", 'B'));
        }
        entries.push(key(if thread.resolved { "Reopen" } else { "Resolve" }, 'x'));
        if folds {
            let fold = if self.doc.fold.collapsed(&thread.root) {
                "Unfold"
            } else {
                "Fold"
            };
            entries.push(self.menu_entry(false, fold, Act::ToggleThread));
        }
        if thread.url.as_deref().is_some_and(is_web) {
            entries.push(key("Open on host", 'o'));
        }
        entries.extend(copies);
        entries
    }

    /// An entry running `action` as its key does, in the tree (`tree`) or
    /// the diff.
    fn menu_entry(&self, tree: bool, label: &str, action: Act) -> menu::Entry {
        menu::Entry {
            label: label.to_owned(),
            key: self.key_for(tree, action).unwrap_or_default(),
            run: Run::Action(action),
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

    /// Input while the command panel is open: a row's key or `enter` runs
    /// it, toggles keeping the panel open with their new state flashing;
    /// see [`Commands`] for the rest.
    fn commands_input(&mut self, input: &Input) -> Option<Action> {
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
            Act::SinceLastReview => toggle(self.since_review()),
            Act::WholeReview => (None, self.whole_review_title(), None),
            Act::OpenReview => (None, format!("Open {}…", self.noun(false)), None),
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
            Act::CycleLayout => {
                if !self.doc.select_other_side() {
                    self.message("these rows have no lines on the other side");
                }
            }
            Act::Select | Act::Cancel => self.doc.selection = None,
            // The panel's copy acts on the selection, so opening it keeps it;
            // a comment goes on it.
            Act::Comment
            | Act::SuggestChange
            | Act::CommentInEditor
            | Act::ScrollLeft
            | Act::ScrollRight
            | Act::ScrollReset
            | Act::CopyPath
            | Act::CopyAbsolutePath
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
            Act::Comment
            | Act::SuggestChange
            | Act::CommentFile
            | Act::Comments
            | Act::NextThread
            | Act::PrevThread
            | Act::ToggleThread
            | Act::ToggleThreads
            | Act::CommentInEditor
            | Act::CommentedFiles => return self.comment_action(action),
            Act::ReviewPull => return self.update_review(),
            Act::ReviewPost => return self.open_post(),
            Act::OpenReview => return self.open_review_picker(),
            Act::SinceLastReview => return self.toggle_since_review(),
            Act::WholeReview => return self.toggle_whole_review(),
            Act::Help => self.help = Some(Help::default()),
            Act::ScrollRight | Act::ScrollLeft | Act::ScrollReset => self.scroll_code(action),
            Act::Select => self.start_selection(),
            Act::Copy if self.doc.note_selection.is_some() => {
                let done = self.copy();
                if done.is_some() {
                    self.doc.note_selection = None;
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
                self.doc.document.structural = !self.doc.document.structural;
                self.remember("syntax-diff", self.doc.document.structural.to_string());
                self.doc.normalize();
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

    /// `/` in the diff: a search line at the bottom, matched as it's typed.
    fn start_search(&mut self) {
        self.doc.selection = None;
        self.search_id += 1;
        let id = self.search_id;
        self.search = Some(Search {
            needle: Arc::new(Needle::new("")),
            editing: true,
            id,
            counts: vec![None; self.doc.document.files().len()],
            pending: None,
        });
        self.live_search.store(id, Ordering::Relaxed);
        self.doc.document.search = None;
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
        search.counts = vec![None; self.doc.document.files().len()];
        search.pending = None;
        self.live_search.store(search.id, Ordering::Relaxed);
        self.doc.document.search = None;
    }

    fn clear_search(&mut self) {
        self.search = None;
        self.live_search.store(u64::MAX, Ordering::Relaxed);
        self.doc.document.search = None;
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
        let current = self
            .doc
            .document
            .diff_of(file)
            .is_some_and(|d| Arc::ptr_eq(d, diff))
            && self.doc.document.hidden_lines(file).as_deref() == Some(hidden);
        let Some(search) = self.search.as_mut().filter(|_| current) else {
            return;
        };
        if let Some(count) = search.counts.get_mut(file) {
            *count = Some(matches.list.len());
        }
        let land = search.pending.take_if(|(f, _)| *f == file);
        self.doc.document.search = Some(Found {
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
        let file = self.doc.document.pos.file;
        let Some(marks) = self
            .doc
            .document
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
        let file = self.doc.document.pos.file;
        let top = Pos {
            offset: 0,
            ..self.doc.document.pos
        };
        let marks = self
            .doc
            .document
            .search
            .as_ref()
            .filter(|m| m.file == file)
            .map(|m| (Arc::clone(&m.matches), m.current));
        // Its diff is coming, or here to scan; not a commit, a failed diff
        // or a combined diff shown as it came, which have no matches.
        let scanned = self.doc.document.diff_of(file).is_some()
            || self
                .doc
                .document
                .files()
                .get(file)
                .is_some_and(view::Entry::wants_diff);
        match marks {
            Some((matches, current)) => {
                let n = matches.list.len();
                // The matches are in the order the diff shows them.
                let mut before = |m: &Match, or_at: bool| {
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
        let shown = self.doc.document.pos.file;
        let unscanned = |f: usize, e: &view::Entry| {
            f == shown && !e.wants_diff() && self.doc.document.diff_of(f).is_none()
        };
        self.doc
            .document
            .files()
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
            .document
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
            self.doc.set_highlight(self.highlight);
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
        self.doc.note_selection = None;
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
        if self.doc.note_selection.is_some() {
            let (text, whole) = self.doc.note_selected()?;
            if text.is_empty() {
                return None;
            }
            let note = if whole {
                "copied a comment".to_owned()
            } else {
                format!(
                    "copied {} of a comment",
                    review::plural(text.chars().count(), "char")
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
        let mut note = format!("copied {}", review::plural(n, unit));
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
                review::plural(thread.notes.len(), "comment")
            );
            return self.copy_text(comments::thread_text(&thread), note);
        }
        let clicked = self
            .tapped_note
            .as_ref()
            .filter(|(root, _)| *root == thread.root)
            .and_then(|(_, id)| thread.notes.iter().find(|n| n.id == *id));
        let text = clicked
            .or_else(|| thread.notes.first())?
            .text
            .trim_end_matches(['\n', '\r']);
        self.copy_text(text.to_owned(), "copied a comment".to_owned())
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
                    self.picker = Some(Picker::new(self.doc.document.files()));
                }
                Some((row, [.., next])) if *row == y && next.contains(&x) => self.doc.next_file(),
                _ => {}
            }
            return;
        }
        self.doc.selection = None;
        self.doc.note_selection = None;
        if self.over_tree(x) {
            self.pane = Pane::Tree;
            // Under the panel's top border.
            let Some(line) = y.checked_sub(tree_top + 1) else {
                return;
            };
            if let Some(panel) = &mut self.panel {
                if let Some(root) = panel.click(line) {
                    self.follow_thread(&root);
                }
                return;
            }
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
        // A tap on a thread focuses it, and on its header also folds or
        // unfolds it; one on code leaves it. A press on a comment's text
        // starts a selection there as one on code does.
        match self.doc.block_row_at(y - 1) {
            Some((Block::Thread(i), header)) => {
                let root = self.doc.threads()[i].root.clone();
                if header {
                    self.doc.fold.toggle(&root);
                    self.doc.refit();
                }
                self.doc.set_focus(Some(root));
                self.tapped_note = self.doc.note_at(y - 1);
                let hit = x
                    .checked_sub(self.content_x())
                    .filter(|_| !header)
                    .and_then(|column| self.doc.note_hit(y - 1, column, None));
                if let Some(hit) = hit {
                    if count > 1 {
                        self.doc.select_word(&hit, count == 3);
                    }
                    self.press = Some(Press {
                        hit,
                        at: (x, y),
                        selected: count > 1,
                    });
                }
                return;
            }
            Some((Block::New, _)) => return,
            None => self.doc.set_focus(None),
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
    /// went down to the cell under it, by characters from code or in the
    /// comment it started in, by lines from the line numbers, and on the
    /// side it started on. Held past the top or bottom of the pane, the
    /// view scrolls and the selection goes on, a line at a time.
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
        if let Hit::Note { thread, note, .. } = press.hit {
            let pane = y.saturating_sub(1);
            self.drag_edge = None;
            if y == 0 || pane >= self.doc.height {
                self.drag_edge = Some((x, y));
                self.doc.scroll(if y == 0 { -1 } else { 1 });
            }
            let column = x.saturating_sub(self.content_x());
            let row = pane.min(self.doc.height.saturating_sub(1));
            if let Some(hit) = self.doc.note_hit(row, column, Some((thread, note))) {
                self.doc.select_to_hit(&hit);
            }
            return;
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
        if press.selected && (self.doc.selection.is_some() || self.doc.note_selection.is_some()) {
            return self.copy();
        }
        None
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

    /// Takes the comments in `path`, reloaded whenever [`Event::Comments`]
    /// says it changed.
    pub(crate) fn set_comments(&mut self, path: PathBuf) {
        let session = self.reviewing.is_some();
        let mut comments = Comments::new(
            path,
            self.local_comments.clone(),
            session,
            self.branch.clone(),
        );
        // On the review plain dffy found, what's written is the working
        // tree's until it's posted.
        comments.set_new_local(self.reviewing.as_ref().is_some_and(|m| m.plain.is_some()));
        self.comments = Some(comments);
        self.reload_comments();
        // A file with no comments reads as unchanged, yet replaces one that
        // may have had a conversation.
        self.sync_conversation();
    }

    /// Takes the local review's file, whose drafts show on a review
    /// session's diff.
    pub(crate) fn set_local_comments(&mut self, path: Option<PathBuf>) {
        self.local_comments = path;
    }

    /// Takes the current branch, before the comments are read: the local
    /// review's threads on other branches stay out of view.
    pub(crate) fn set_branch(&mut self, branch: Option<String>) {
        self.branch = branch;
    }

    /// The files [`App::set_comments`] with `path` reads, to watch.
    pub(crate) fn comment_files(&self, path: PathBuf) -> Vec<PathBuf> {
        Comments::new(path, self.local_comments.clone(), false, None).sources()
    }

    /// Reads the comments file again, if it changed since the last read,
    /// and places the threads that changed on the files already diffed.
    /// Says the file's notice, if it has one. On a review, drafts changed
    /// elsewhere are planned again.
    fn reload_comments(&mut self) {
        let Some(comments) = &mut self.comments else {
            return;
        };
        let drafts = |c: &Comments| -> Vec<(String, bool)> {
            c.drafts()
                .into_iter()
                .map(|(id, _, refused)| (id.to_owned(), refused))
                .collect()
        };
        let before = self.reviewing.as_ref().map(|_| drafts(comments));
        match comments.refresh() {
            Ok(None) => {}
            Ok(Some(changed)) => {
                if before.is_some_and(|before| before != drafts(comments)) {
                    self.plan_soon();
                }
                self.placed_again(&changed);
                if let Some(notice) = self.comments.as_ref().and_then(Comments::notice) {
                    self.message(escape(notice.as_bytes()));
                }
            }
            Err(e) => self.message(format!("comments: {e}")),
        }
    }

    /// After `changed` paths' comments changed: finds them in the list and
    /// places their threads on those diffed; the others wait for their
    /// diffs.
    fn placed_again(&mut self, changed: &[BString]) {
        self.sync_conversation();
        let Some(comments) = &mut self.comments else {
            return;
        };
        comments.locate(self.doc.document.files());
        let files: Vec<usize> = changed.iter().filter_map(|p| comments.file_of(p)).collect();
        if self.tree.commented() {
            self.tree.set_commented(Some(comments.paths()));
        }
        for file in files {
            self.place(file);
        }
        self.dirty = true;
    }

    /// Puts the conversation page first while a review session's comments
    /// have general comments, sized to them, and takes it away when they
    /// have none. The page's threads are made once it shows. While no file
    /// is listed it waits, so the viewer still opens on the first file.
    fn sync_conversation(&mut self) {
        let count = self
            .comments
            .as_ref()
            .filter(|_| self.reviewing.is_some())
            .map_or(0, Comments::conversation_len);
        let files = self.doc.document.files();
        let shown = files.first().map(view::Entry::source);
        let shown = match shown {
            Some(Source::Page { rows }) if *rows == count + 1 => return,
            Some(Source::Page { .. }) => true,
            _ => false,
        };
        if !shown && (count == 0 || files.is_empty()) {
            return;
        }
        if let Some(mode) = &self.reviewing {
            let title = mode.session.title.as_deref().unwrap_or(&mode.label);
            title.clone_into(&mut self.doc.page_title);
        }
        let moved = self.doc.set_page((count > 0).then_some(count));
        if moved != 0 {
            self.files_moved(moved);
        }
        self.dirty = true;
        self.kept = false;
    }

    /// After every entry moved by `by`, the page having come or gone: what
    /// holds a file by its place in the list follows it.
    fn files_moved(&mut self, by: isize) {
        let shift = |file: &mut usize| *file = file.saturating_add_signed(by);
        if let Some(search) = &mut self.search {
            if by > 0 {
                search.counts.insert(0, None);
            } else if !search.counts.is_empty() {
                search.counts.remove(0);
            }
            if let Some((file, _)) = &mut search.pending {
                shift(file);
            }
        }
        if let Some(target) = &mut self.render {
            shift(&mut target.file);
        }
        if let Some((file, _)) = &mut self.thread_pending {
            shift(file);
        }
        let visible = &mut self.visible;
        *visible = visible.start.saturating_add_signed(by)..visible.end.saturating_add_signed(by);
        // Positions in the old list, a click's or a tap's, are dropped.
        self.tapped = None;
        self.press = None;
        self.rebuild_tree();
        if let Some(picker) = &mut self.picker {
            picker.rebuild(self.doc.document.files());
        }
        self.sync_threads();
    }

    /// Places the threads of `file`, diffed or failed to be, if it has any,
    /// and reveals the lines of each, folded or not, that a gap hid, so it
    /// hangs under its own line; positions stay on their lines.
    fn place(&mut self, file: usize) {
        let highlight = self.highlight;
        let (Some(comments), Some(entry)) =
            (&mut self.comments, self.doc.document.files().get(file))
        else {
            return;
        };
        let path = &entry.change().path;
        if !comments.has(path) {
            return;
        }
        let threads = match entry.state() {
            State::Loaded(loaded) => {
                let diff = comments::line_diff(loaded);
                let texts = Some([&diff.old, &diff.new]);
                comments.place(path, entry.version(), texts, highlight);
                comments.threads(path)
            }
            State::Failed(_) => {
                comments.place(path, entry.version(), None, highlight);
                None
            }
            State::Pending => return,
        };
        let mut revealed = false;
        for thread in threads.iter().flat_map(|t| t.iter()) {
            if let comments::At::Line { side, lines } = &thread.at {
                let document = &mut self.doc.document;
                revealed |= document.reveal_lines(file, *side, lines.clone(), THREAD_CONTEXT);
            }
        }
        if revealed {
            self.doc.normalize();
            self.dirty = true;
            self.kept = false;
        }
        // `m`, `M` or the panel went here before its threads were.
        if self.doc.document.pos.file == file
            && let Some((_, seek)) = self.thread_pending.take_if(|(pending, _)| *pending == file)
        {
            self.sync_threads();
            match seek {
                Seek::Edge(last) => self.doc.edge_thread(last),
                Seek::Root(root) => self.doc.go_to_thread(&root),
            };
        }
    }

    /// Writes `ops` to the comments file and says `note`, or why not.
    /// Returns the id each operation touched, `None` when it failed.
    fn write_comments(&mut self, ops: Vec<Op>, note: &str) -> Option<Vec<Option<String>>> {
        let comments = self.comments.as_mut()?;
        match comments.apply(ops) {
            Ok((changed, ids)) => {
                self.plan_soon();
                self.placed_again(&changed);
                self.message(note);
                Some(ids)
            }
            Err(e) => {
                self.message(format!("comments: {e}"));
                None
            }
        }
    }

    /// The editor closed with `text` written, or `None` when it failed:
    /// saves what was being written, unless it's empty or unchanged. Text
    /// the box handed over opens in the box again when the editor failed.
    /// When the save fails, the text opens in the box to try again where
    /// the diff shows its place; else it's returned, for the caller to
    /// keep.
    pub(crate) fn composed(&mut self, text: Option<String>) -> Option<String> {
        let handed = self.handed.take();
        let compose = self.composing.take()?;
        let Some(text) = text else {
            if let Some((text, at)) = handed {
                self.open_box(compose, at, &text);
            }
            return None;
        };
        let at = handed.map(|(_, at)| at).or_else(|| self.box_for(&compose));
        let reopens = at.is_some();
        (!self.save(compose, at, &text) && !reopens).then_some(text)
    }

    /// Where the text box for `compose` goes, when the diff shows it: a new
    /// comment's file shown, or the thread replied in or edited.
    fn box_for(&self, compose: &Compose) -> Option<Writes> {
        let shown = |root: &str| self.doc.threads().iter().any(|t| t.root == root);
        match compose {
            Compose::New(comment) => {
                let entry = self.doc.document.files().get(self.doc.document.pos.file)?;
                if entry.change().path != comment.path.as_bytes() {
                    return None;
                }
                Some(match comment.range() {
                    Some((side, lines)) => Writes::New {
                        side: match side {
                            crate::review::Side::Old => Side::Old,
                            crate::review::Side::New => Side::New,
                        },
                        lines,
                    },
                    None => Writes::File,
                })
            }
            Compose::Reply(root) => shown(root).then(|| Writes::Reply(root.clone())),
            Compose::Edit { id, .. } => {
                let root = self.comments.as_ref()?.thread_root(id)?;
                shown(&root).then(|| Writes::Edit {
                    root,
                    id: id.clone(),
                })
            }
        }
    }

    /// Saves `text` as `compose` says, and focuses its thread. When the
    /// write fails, the text stays in the box (`at`) to try again. Returns
    /// whether it was saved, or there was nothing to save.
    fn save(&mut self, compose: Compose, at: Option<Writes>, text: &str) -> bool {
        let root = match &compose {
            Compose::New(_) => None,
            Compose::Reply(root) => Some(root.clone()),
            Compose::Edit { id, .. } => self.comments.as_ref().and_then(|c| c.thread_root(id)),
        };
        let edit = matches!(compose, Compose::Edit { .. });
        let retry = at.map(|at| (compose.clone(), at));
        let Some((op, note)) = compose.into_op(text) else {
            self.message(if edit {
                "nothing changed; nothing saved"
            } else {
                "nothing written; nothing saved"
            });
            return true;
        };
        if let Some(ids) = self.write_comments(vec![op], note) {
            let root = root.or_else(|| ids.into_iter().next().flatten());
            self.sync_threads();
            if let Some(root) = root {
                self.doc.focus_thread(&root);
            }
            return true;
        }
        if let Some((compose, at)) = retry {
            self.open_box(compose, at, text);
            self.save_failed = true;
            if let Some((message, _)) = &mut self.message {
                *message = format!("not saved, esc again discards · {message}");
            }
        }
        false
    }

    fn open_box(&mut self, compose: Compose, at: Writes, text: &str) {
        if let Writes::Reply(root) | Writes::Edit { root, .. } = &at {
            self.doc.fold.keep_open(root);
        }
        self.composing = Some(compose);
        self.doc.writing = Some(Writing {
            at,
            text: TextBox::new(text),
        });
        self.doc.selection = None;
        self.doc.refit();
        self.reveal_box();
    }

    /// Scrolls the text box into view, through its cursor.
    fn reveal_box(&mut self) {
        if let Some(block) = self.doc.writing_block() {
            self.doc.reveal_block(block, true);
        }
    }

    /// Input while the text box is open: every key edits it but `esc`,
    /// which saves it (an empty text discards it, and so does a second
    /// `esc` after a save failed), `ctrl+g`, which hands it to the editor,
    /// and `ctrl+c`, which saves it and quits. The wheel still scrolls.
    fn writing_input(&mut self, input: Input) -> Option<Action> {
        let ctrl = |mods: Mods| mods.contains(Mods::CTRL);
        let failed = std::mem::take(&mut self.save_failed);
        match input {
            Input::Key(Key::Esc, _) if failed => {
                self.doc.writing = None;
                self.composing = None;
                self.doc.refit();
                self.message("discarded");
            }
            Input::Key(Key::Esc, _) => self.close_box(),
            Input::Key(Key::Char('c'), mods) if ctrl(mods) => {
                self.close_box();
                return Some(Action::Quit);
            }
            // Not ctrl+e: Ghostty sends it for cmd+right, which the box
            // takes as the end of the row.
            Input::Key(Key::Char('g'), mods) if ctrl(mods) => {
                let writing = self.doc.writing.take()?;
                let text = writing.text.text().to_owned();
                self.handed = Some((text.clone(), writing.at));
                self.doc.refit();
                return Some(Action::Compose(text));
            }
            Input::Key(key, mods) => {
                if let Some(writing) = &mut self.doc.writing {
                    writing.text.key(key, mods);
                }
                self.reveal_box();
            }
            Input::Paste(text) => {
                if let Some(writing) = &mut self.doc.writing {
                    writing.text.paste(&text);
                }
                self.reveal_box();
            }
            Input::Scroll { delta, .. } => self.scroll_diff(isize::from(delta) * WHEEL_ROWS),
            _ => {}
        }
        None
    }

    /// Closes the text box, saving its text.
    fn close_box(&mut self) {
        let Some(writing) = self.doc.writing.take() else {
            return;
        };
        self.doc.refit();
        let Some(compose) = self.composing.take() else {
            return;
        };
        let text = writing.text.text().to_owned();
        if text.trim().is_empty() && !matches!(compose, Compose::Edit { .. }) {
            self.message("empty; nothing saved");
            return;
        }
        self.save(compose, Some(writing.at), &text);
    }

    /// A comment that's to go on the current branch gets it: one in the
    /// local review's file, which keeps branches apart.
    fn on_branch(&self, mut comment: crate::review::Comment) -> crate::review::Comment {
        if self.comments.as_ref().is_some_and(Comments::branched) {
            comment.branch.clone_from(&self.branch);
        }
        comment
    }

    /// Whether comments can be written here; says why not.
    fn can_comment(&mut self) -> bool {
        if self.comments.is_some() {
            return true;
        }
        self.message(if self.piped {
            "a piped diff can't have comments"
        } else {
            "no comments file here: pass --comments <file>"
        });
        false
    }

    /// A new comment, without its text, on the selected lines or the line
    /// `v` would start on.
    fn new_comment(&mut self) -> Option<(Compose, Writes)> {
        let tapped = self.take_tapped();
        let target = self.doc.comment_target(tapped);
        self.doc.selection = None;
        let Some((file, side, lines)) = target else {
            self.message("no line of code here to comment on");
            return None;
        };
        let entry = &self.doc.document.files()[file];
        let Some(loaded) = entry.file().filter(|_| !entry.is_commit()) else {
            self.message("no line of code here to comment on");
            return None;
        };
        match comments::draft(
            entry.change(),
            comments::line_diff(loaded),
            side,
            lines.clone(),
        ) {
            Ok(comment) => Some((
                Compose::New(Box::new(self.on_branch(comment))),
                Writes::New { side, lines },
            )),
            Err(why) => {
                self.message(why);
                None
            }
        }
    }

    /// `c`: the text box for a comment on the selected lines, or the line
    /// `v` would start on, right under them.
    fn comment(&mut self) {
        if !self.can_comment() {
            return;
        }
        if let Some((compose, at)) = self.new_comment() {
            self.doc.set_focus(None);
            self.open_box(compose, at, "");
        }
    }

    /// "Suggest change": [`App::comment`]'s box, holding the lines as a
    /// suggestion to edit, the cursor above it for what to say.
    fn suggest(&mut self) {
        if !self.can_comment() {
            return;
        }
        let Some((compose, at)) = self.new_comment() else {
            return;
        };
        let Compose::New(comment) = &compose else {
            return;
        };
        let lines = comment
            .anchor
            .as_ref()
            .map(|a| a.lines.join("\n"))
            .unwrap_or_default();
        let text = comments::with_suggestion("", Some(&lines));
        self.doc.set_focus(None);
        self.open_box(compose, at, &format!("\n\n{text}"));
        if let Some(writing) = &mut self.doc.writing {
            writing.text.rewind();
        }
    }

    /// `C`: the text box for a comment on the whole file shown, under its
    /// first row.
    fn comment_file(&mut self) {
        if !self.can_comment() {
            return;
        }
        let Some(entry) = self.doc.document.files().get(self.doc.document.pos.file) else {
            return;
        };
        if entry.is_commit() {
            self.message(if entry.is_page() {
                "the conversation is read-only here for now"
            } else {
                "a commit has no file to comment on"
            });
            return;
        }
        match comments::file_draft(entry.change()) {
            Ok(comment) => {
                // The box is in the diff, which a tree over it would hide.
                if self.pane == Pane::Tree {
                    self.leave_panel();
                }
                self.doc.set_focus(None);
                let comment = self.on_branch(comment);
                self.open_box(Compose::New(Box::new(comment)), Writes::File, "");
            }
            Err(why) => self.message(why),
        }
    }

    /// The command panel's "Continue in editor": your last draft in the
    /// focused thread (or the one selected in the comments panel), else a
    /// reply to it, else a new comment on the line, written in the editor.
    /// The text box is the way in from a phone: `esc` saves it and focuses
    /// its thread, and this takes it from there.
    fn comment_in_editor(&mut self) -> Option<Action> {
        if !self.can_comment() {
            return None;
        }
        let target = self.target_thread();
        if target
            .as_ref()
            .is_some_and(|t| matches!(t.at, comments::At::Conversation { .. }))
        {
            self.message("the conversation is read-only here for now; o opens it on the host");
            return None;
        }
        let focused = target.map(|t| {
            let draft = t.notes.iter().rev().find(|n| !n.posted());
            (
                t.root.clone(),
                draft.map(|n| {
                    let text = comments::with_suggestion(&n.text, n.suggestion.as_deref());
                    (n.id.clone(), text)
                }),
            )
        });
        let (compose, text) = match focused {
            Some((_, Some((id, text)))) => (
                Compose::Edit {
                    id,
                    before: text.clone(),
                },
                text,
            ),
            Some((root, None)) => (Compose::Reply(root), String::new()),
            None => (self.new_comment()?.0, String::new()),
        };
        self.composing = Some(compose);
        Some(Action::Compose(text))
    }

    /// The thread the thread keys act on: the comments panel's selected
    /// one while it has the keys, else the one focused in the diff.
    fn target_thread(&self) -> Option<comments::Thread> {
        match (&self.panel, self.pane) {
            (Some(panel), Pane::Tree) => panel.thread().cloned(),
            _ => self.doc.focused().cloned(),
        }
    }

    /// A key for [`App::target_thread`]. `Err` for keys it doesn't take,
    /// which act as they would without a thread; `deleting` after a first
    /// `d`.
    fn thread_key(&mut self, key: Key, mods: Mods, deleting: bool) -> Result<Option<Action>, ()> {
        let thread = self
            .target_thread()
            .filter(|_| mods == Mods::NONE)
            .ok_or(())?;
        let draft = thread.notes.iter().rev().find(|n| !n.posted());
        let general = matches!(thread.at, comments::At::Conversation { .. });
        let mut done = None;
        match key {
            // The selection ends below; the focus stays for the next `esc`.
            Key::Esc if self.doc.note_selection.is_some() => {}
            Key::Esc => self.doc.set_focus(None),
            Key::Char('r' | 'x') if general => {
                self.message("the conversation is read-only here for now; o opens it on the host");
            }
            Key::Char('r') => {
                let warning = self
                    .naming
                    .as_ref()
                    .and_then(|n| n.outdated_reply_warning.as_deref())
                    .filter(|_| thread.outdated && !self.warned_outdated);
                if let Some(warning) = warning {
                    self.warned_outdated = true;
                    self.message(format!("outdated: {}", escape(warning.as_bytes())));
                }
                let root = thread.root.clone();
                done = self.write_in(
                    &thread.root,
                    Compose::Reply(root.clone()),
                    Writes::Reply(root),
                    "",
                );
            }
            Key::Char('e') => match draft {
                Some(note) => {
                    let text = comments::with_suggestion(&note.text, note.suggestion.as_deref());
                    done = self.write_in(
                        &thread.root,
                        Compose::Edit {
                            id: note.id.clone(),
                            before: text.clone(),
                        },
                        Writes::Edit {
                            root: thread.root.clone(),
                            id: note.id.clone(),
                        },
                        &text,
                    );
                }
                None => self.message("no draft here to edit; posted comments are the host's"),
            },
            Key::Char('x') => {
                let change = Change {
                    resolved: Some(!thread.resolved),
                    ..Change::default()
                };
                let note = if thread.resolved {
                    "thread reopened"
                } else {
                    "thread resolved"
                };
                let op = Op::Set {
                    target: Target::Id(thread.root.clone()),
                    change,
                };
                self.write_comments(vec![op], note);
            }
            Key::Char('a') if thread.unposted() => self.adopt(&thread),
            Key::Char('B') if thread.branch.is_some() => self.move_here(&thread.root),
            Key::Char('d') => match draft {
                None => self.message("no draft here to delete; posted comments are the host's"),
                Some(note) if deleting => {
                    let gone = if note.id == thread.root {
                        "thread deleted"
                    } else {
                        "draft deleted"
                    };
                    self.write_comments(vec![Op::Rm(Target::Id(note.id.clone()))], gone);
                }
                Some(note) => {
                    self.deleting = true;
                    self.message(if note.id == thread.root {
                        "d again deletes this thread, replies too · any other key keeps it"
                    } else {
                        "d again deletes your draft reply · any other key keeps it"
                    });
                }
            },
            Key::Char('o') => match thread.url {
                Some(url) if is_web(&url) => return Ok(Some(Action::Browse(url))),
                Some(_) => self.message("this thread's link isn't a web address"),
                None => self.message("this thread has no link: it isn't posted"),
            },
            _ => return Err(()),
        }
        self.doc.note_selection = None;
        Ok(done)
    }

    /// `a`: someone else's comments in `thread`, an agent's, become yours:
    /// drafts that posting sends under your name.
    fn adopt(&mut self, thread: &comments::Thread) {
        let ops: Vec<Op> = (thread.notes.iter())
            .filter(|n| n.standing == comments::Standing::Unposted)
            .map(|n| Op::Set {
                target: Target::Id(n.id.clone()),
                change: Change {
                    author: Some(String::new()),
                    ..Change::default()
                },
            })
            .collect();
        let note = format!("now yours: {}", review::plural(ops.len(), "draft"));
        self.write_comments(ops, &note);
    }

    /// `B`: the thread with root `root`, on another branch, moves to the
    /// current one.
    fn move_here(&mut self, root: &str) {
        let op = Op::Set {
            target: Target::Id(root.to_owned()),
            change: Change {
                branch: Some(self.branch.clone()),
                ..Change::default()
            },
        };
        let note = match &self.branch {
            Some(branch) => format!("moved to {}", escape(branch.as_bytes())),
            None => "moved to no branch: it shows on every one".to_owned(),
        };
        self.write_comments(vec![op], &note);
    }

    /// Opens the text box `at` in the thread with root `root` when the
    /// diff shows that thread, else hands `text` to the editor: the box is
    /// drawn only inside its thread in the diff. From the comments panel
    /// the diff follows the thread first, and takes the keys for the box.
    fn write_in(&mut self, root: &str, compose: Compose, at: Writes, text: &str) -> Option<Action> {
        let shown = |app: &Self| app.doc.focused().is_some_and(|t| t.root == root);
        if self.pane == Pane::Tree && self.panel.is_some() {
            if !shown(self) {
                self.follow_thread(root);
            }
            if shown(self) {
                self.leave_panel();
            }
        }
        if shown(self) {
            self.open_box(compose, at, text);
            return None;
        }
        self.composing = Some(compose);
        Some(Action::Compose(text.to_owned()))
    }

    fn comment_action(&mut self, action: Act) -> Option<Action> {
        match action {
            Act::Comment => self.comment(),
            Act::SuggestChange => self.suggest(),
            Act::CommentFile => self.comment_file(),
            Act::Comments => self.toggle_comments(),
            Act::NextThread | Act::PrevThread => {
                self.doc.selection = None;
                self.step_thread(action == Act::NextThread);
            }
            Act::ToggleThread => self.toggle_thread(),
            Act::ToggleThreads => self.toggle_threads(),
            Act::CommentInEditor => return self.comment_in_editor(),
            Act::CommentedFiles => self.toggle_commented(),
            _ => {}
        }
        None
    }

    /// `m` and `M`: the next or previous thread, on into the next or
    /// previous file with comments, focused.
    fn step_thread(&mut self, forward: bool) {
        let Some(comments) = &self.comments else {
            self.message("no comments file here: pass --comments <file>");
            return;
        };
        // An update's new threads come first, in list order, while any of
        // them is still there.
        while forward && !self.new_threads.is_empty() {
            let root = self.new_threads.remove(0);
            if comments.thread_root(&root).is_none() {
                continue;
            }
            let left = self.new_threads.len();
            self.follow_thread(&root);
            if left > 0 && self.message.is_none() {
                self.message(format!("{left} more new"));
            }
            return;
        }
        let files = comments.files();
        if self.doc.step_thread(forward) {
            return;
        }
        let here = self.doc.document.pos.file;
        let next = if forward {
            files.iter().copied().find(|&f| f > here)
        } else {
            files.iter().copied().rev().find(|&f| f < here)
        };
        let Some(file) = next else {
            self.message(if forward {
                "no more threads below"
            } else {
                "no more threads above"
            });
            return;
        };
        self.doc.set_focus(None);
        self.doc.goto_file(file);
        self.sync_threads();
        if !self.doc.edge_thread(!forward) {
            self.thread_pending = Some((file, Seek::Edge(!forward)));
            self.message("waiting for this file's diff");
            return;
        }
        let (arrow, what) = if forward {
            ("→", "next file")
        } else {
            ("←", "previous file")
        };
        self.note_file(arrow, what, here, file);
    }

    /// `z`: folds the focused thread, or the first on screen, or unfolds
    /// it.
    fn toggle_thread(&mut self) {
        let Some(i) = self.doc.nearest_thread() else {
            self.message("no thread here; m goes to the next");
            return;
        };
        let root = self.doc.threads()[i].root.clone();
        self.doc.fold.toggle(&root);
        self.doc.refit();
    }

    /// `Z`: every thread folded, or every one unfolded.
    fn toggle_threads(&mut self) {
        let folded = self.doc.fold.toggle_all();
        self.doc.refit();
        self.message(if folded {
            "threads folded; z unfolds one"
        } else {
            "threads unfolded"
        });
    }

    /// The tree keeps only the files with comments, or every file again.
    fn toggle_commented(&mut self) {
        let Some(comments) = &self.comments else {
            self.message("no comments file here: pass --comments <file>");
            return;
        };
        let on = !self.tree.commented();
        self.tree.set_commented(on.then(|| comments.paths()));
        self.close_panel();
        if on {
            self.focus_tree();
            if let Some(Item::File(file)) = self.tree.step(0) {
                self.doc.goto_file(file);
            }
        }
        self.message(if on {
            "the tree shows files with comments; f/F and n/N keep to them"
        } else {
            "the tree shows every file"
        });
    }

    /// `i`: the left panel shows every comment thread, with the keys,
    /// starting on the focused one or the first of the file shown; or,
    /// when it shows them, the files again.
    fn toggle_comments(&mut self) {
        if self.panel.is_some() && self.tree_shown {
            self.close_panel();
            return;
        }
        if self.panel.is_none() {
            if !self.can_comment() {
                return;
            }
            let Some(comments) = &self.comments else {
                return;
            };
            let root = self.doc.focused().map(|t| t.root.as_str());
            let file = self.doc.document.files().get(self.doc.document.pos.file);
            let path = file.map_or(&[][..], |e| &e.change().path[..]);
            self.panel = Some(Panel::new(comments, self.enumerating, (root, path)));
        }
        self.focus_tree();
    }

    /// Closes the comments panel, ending the peek its selection made.
    /// Returns whether it was open.
    fn close_panel(&mut self) -> bool {
        self.doc.peek(None);
        self.panel.take().is_some()
    }

    /// A key for the comments panel, when it has the keys: the thread keys
    /// act on the selected thread, as on a focused one in the diff (see
    /// [`App::thread_key`]). `Continue` when it's neither's.
    fn panel_key(&mut self, key: Key, mods: Mods, deleting: bool) -> ControlFlow<Option<Action>> {
        let (Some(panel), Some(comments), Pane::Tree) =
            (&mut self.panel, &self.comments, self.pane)
        else {
            return ControlFlow::Continue(());
        };
        match panel.key((comments, self.enumerating), &self.keys, key, mods) {
            comments::Outcome::Stay => {}
            comments::Outcome::Moved(root) => self.follow_thread(&root),
            comments::Outcome::Diff => self.leave_panel(),
            comments::Outcome::Pass => {
                return match self.thread_key(key, mods, deleting) {
                    Ok(done) => ControlFlow::Break(done),
                    Err(()) => ControlFlow::Continue(()),
                };
            }
        }
        ControlFlow::Break(None)
    }

    /// Shows the thread with root `root` in the diff, focused and peeked
    /// at, its file loaded when it's another, and leaves the keys where
    /// they are; a thread not placed yet is focused once it is. One on a
    /// file not in the diff leaves nothing focused: the panel's header
    /// says so.
    fn follow_thread(&mut self, root: &str) {
        // A `d` armed on the thread selected before would delete in this one.
        self.deleting = false;
        let Some(comments) = &self.comments else {
            return;
        };
        // On another branch: only the panel lists it.
        if comments.elsewhere(root) {
            self.doc.peek(None);
            self.doc.set_focus(None);
            return;
        }
        let path = comments.home_of(root).unwrap_or_default().to_owned();
        self.doc.selection = None;
        self.thread_pending = None;
        self.doc.peek(Some(root));
        // A general comment is on the conversation page, the first entry.
        let page = (self.doc.document.files().first())
            .is_some_and(view::Entry::is_page)
            .then_some(0);
        let Some(file) = (if path.is_empty() {
            page
        } else {
            comments.file_of(path.as_bytes())
        }) else {
            self.doc.set_focus(None);
            if self.enumerating {
                self.message("still listing files; its file isn't here yet");
            }
            return;
        };
        self.doc.goto_file(file);
        self.sync_threads();
        if !self.doc.go_to_thread(root) {
            self.doc.set_focus(None);
            self.thread_pending = Some((file, Seek::Root(root.to_owned())));
            self.message("waiting for this file's diff");
        }
    }

    /// The search's scan of the file shown, once it's diffed and until its
    /// matches are in for the lines it shows now.
    fn find_job(&self) -> Option<Job> {
        let search = self.search.as_ref().filter(|s| !s.needle.text.is_empty())?;
        let file = self.doc.document.pos.file;
        let hidden = self.doc.document.hidden_lines(file)?;
        if self
            .doc
            .document
            .search
            .as_ref()
            .is_some_and(|m| m.file == file && m.hidden == hidden)
        {
            return None;
        }
        Some(Job::Find {
            file,
            version: self.version(file),
            diff: Arc::clone(self.doc.document.diff_of(file)?),
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
        let files = self.doc.document.files();
        let diff = |file: usize| {
            files[file].wants_diff().then(|| Job::Diff {
                file,
                version: files[file].version(),
                change: files[file].change().clone(),
                piped: match &files[file].source() {
                    Source::Piped(piped) => Some(Arc::clone(piped)),
                    Source::Repo | Source::Commit(_) | Source::Page { .. } => None,
                },
            })
        };
        let mut jobs: Vec<Job> = self.visible.clone().filter_map(diff).collect();
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
        if let Some(comments) = &self.comments {
            jobs.extend(
                comments
                    .pending(files)
                    .into_iter()
                    .filter(|f| !near.contains(f))
                    .filter_map(diff),
            );
        }
        jobs.extend(self.search_jobs());
        jobs
    }

    /// The search's counts of the files not shown, nearest after the one
    /// shown first.
    fn search_jobs(&self) -> Vec<Job> {
        let Some(search) = self.search.as_ref().filter(|s| !s.needle.text.is_empty()) else {
            return Vec::new();
        };
        let files = self.doc.document.files();
        let n = files.len();
        let start = self.doc.document.pos.file;
        (1..n)
            .map(|k| (start + k) % n)
            .filter(|&f| !files[f].is_commit() && search.counts.get(f).is_none_or(Option::is_none))
            .take(SEARCH_AHEAD)
            .map(|file| Job::Search {
                file,
                version: files[file].version(),
                change: files[file].change().clone(),
                piped: match &files[file].source() {
                    Source::Piped(piped) => Some(Arc::clone(piped)),
                    Source::Repo | Source::Commit(_) | Source::Page { .. } => None,
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
            .document
            .files()
            .get(doc.document.pos.file)
            .and_then(|e| match &e.state() {
                State::Loaded(l) if l.diff().note.is_some() => l.diff().note.clone(),
                State::Loaded(l) if doc.document.structural => {
                    l.structural_note().map(str::to_owned)
                }
                _ => None,
            })
            .or_else(|| self.render_note());
        let status = chrome::Status {
            chip: chip(self),
            position: position(self).filter(|_| !narrow),
            hunk: doc
                .document
                .hunk_position()
                .map(|(i, n)| format!("{i}/{n} hunk")),
            middle: note.clone().or_else(|| Some(modes(self))),
            note: note.is_some(),
            review: review_status(self),
            search: [
                self.search_status(),
                (!self.tree.filter().is_empty() || self.tree.commented())
                    .then(|| "filter".to_owned()),
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
            tree_editing: self.tree.editing || self.panel.as_ref().is_some_and(|p| p.editing),
            heading: heading(self),
            context: self.context(),
            status,
        }
    }

    /// The bytes for the terminal when something changed since the last
    /// frame.
    #[expect(
        clippy::too_many_lines,
        reason = "the frame's layers in the order they're drawn"
    )]
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
            let file = self.doc.document.pos.file;
            let here = match &self.doc.document.search {
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
        let empty = self
            .doc
            .document
            .files()
            .is_empty()
            .then(|| self.empty_note());
        let overlays = self.picker.is_some()
            || self.help.is_some()
            || self.commands.is_some()
            || self.reviews.is_some()
            || self.posting.is_some()
            || self.menu.is_some();
        self.sync_threads();
        self.doc.now = now();
        let sending =
            (self.posting.as_ref()).map(|d| (self.sending(d.with_local()), self.plan_checked()));
        let (tree_top, tree_rows) = self.tree_rows();
        let picker_top = self.picker_top();
        let (grid, last) = self.screen.frame();
        chrome.outline(grid, &self.theme);
        let last = std::mem::take(&mut self.kept).then_some(last);
        let (visible, scrolled) = self
            .doc
            .draw(grid, last, (x, width, 1), &self.theme, self.frame);
        self.visible = visible;
        // The picker covers the tree, and refreshing it costs a pass over
        // every file while the picker reads a long input.
        if let Some(columns) = tree_columns.filter(|_| self.picker.is_none()) {
            if chrome.overlay {
                // Blank out the diff under it.
                for y in tree_top..tree_top + tree_rows {
                    grid.fill(0, y, columns, Style::PLAIN);
                }
            }
            let place = (tree_top, tree_rows, columns);
            let (focused, joined) = (self.pane == Pane::Tree, framed && !chrome.overlay);
            if let (Some(panel), Some(comments)) = (&mut self.panel, &self.comments) {
                // As the tree follows the file, `m` or a tap in the diff
                // moves the selection.
                if let Some(thread) = self.doc.focused() {
                    panel.select(&thread.root);
                }
                let comments = (comments, self.enumerating);
                panel.draw(grid, place, comments, (focused, joined), &self.theme);
            } else {
                let files = self.doc.document.files();
                if self.pane == Pane::Diff && !files.is_empty() {
                    self.tree.follow(self.doc.document.pos.file);
                }
                let totals = Totals::of(files, self.enumerating);
                self.tree.draw(
                    grid,
                    place,
                    files,
                    &totals,
                    self.comments.as_ref(),
                    self.enumerating,
                    focused,
                    joined,
                    &self.theme,
                );
            }
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
            } else if self.doc.document.files().len() >= piped::MAX_ENTRIES {
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
        if let Some(picker) = &mut self.reviews {
            picker.draw(grid, &self.theme);
        }
        if let (Some(dialog), Some(mode), Some(sending)) =
            (&mut self.posting, &self.reviewing, sending)
        {
            dialog.draw(grid, &self.theme, mode, sending);
        }
        if let Some(menu) = &self.menu {
            menu.draw(grid, &self.theme);
        }
        self.kept = !overlays && !chrome.overlay;
        // The terminal scrolls whole rows, so the cell diff sends the tree's
        // columns again on every row it scrolled: worth it while they're
        // fewer than the diff's cells that moved. Tree cells count a third
        // more, the ratio at which a full tree beside short code lines
        // never costs more than the plain diff; a mostly blank tree would
        // gain on larger scrolls too, which counting the cells that differ
        // would catch.
        Some(self.screen.flush(scrolled.filter(|(rows, by)| {
            3 * (width - x) * (rows.len() - by.unsigned_abs()) > 4 * x * rows.len()
        })))
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
    picker.catch_up(doc.document.files());
    let rows = picker.rows(
        doc.document.files(),
        doc.document.commits(),
        (width, rows_shown),
        progress,
    );
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
    if doc.document.structural {
        modes.push_str(" syntax");
    }
    // Only on files it applies to: elsewhere it would be noise.
    let renders = doc
        .document
        .files()
        .get(doc.document.pos.file)
        .is_some_and(|e| {
            matches!(e.source(), Source::Repo) && app.renderers.for_path(&e.change().path).is_some()
        });
    if doc.document.rendered() && renders {
        modes.push_str(" rendered");
    }
    if doc
        .document
        .files()
        .get(doc.document.pos.file)
        .is_some_and(|e| doc.document.is_full(e))
    {
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
        let leader = || {
            let (chord, _) = app
                .keys
                .leader_entries()
                .iter()
                .find(|(_, a)| *a == Act::ToggleMouse)?;
            Some(format!("{} {}", app.key_name(Act::Leader)?, chord.name()))
        };
        let key = app
            .keys
            .chords(false, Act::ToggleMouse)
            .next()
            .map(crate::keys::Chord::name)
            .or_else(leader)
            .map(|k| format!(" ({k})"))
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
    match doc.document.commit_of(doc.document.pos.file) {
        Some(c) if doc.document.commits().len() > 1 || app.piped => {
            format!("commit {}/{}{growing}", c + 1, doc.document.commits().len())
        }
        _ => app.description.clone(),
    }
}

/// Which file of how many, or under a commit, which of its files (on the
/// commit itself, how many it has). A `+` marks counts still growing.
fn position(app: &App) -> Option<String> {
    let doc = &app.doc;
    if doc.document.files().is_empty() {
        return None;
    }
    let growing = if app.enumerating { "+" } else { "" };
    let file = doc.document.pos.file;
    let Some(c) = doc.document.commit_of(file) else {
        let total = doc.document.files().len();
        return Some(format!("{}/{total}{growing}", (file + 1).min(total)));
    };
    let entry = doc.document.commits()[c];
    let files = doc.document.commit_files(entry).len();
    let last = if c + 1 == doc.document.commits().len() {
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
    let file = doc.document.pos.file;
    match doc.document.commit_of(file) {
        Some(c) if doc.document.commits()[c] == file => {
            format!("{}/{}{growing}", c + 1, doc.document.commits().len())
        }
        Some(_) => position(app).unwrap_or_default(),
        None => {
            let total = doc.document.files().len();
            format!("{}/{total}{growing}", (file + 1).min(total))
        }
    }
}

/// The file shown, for the title: on wide screens its state, staging,
/// path and counts (a commit's id and subject), on narrow ones the file
/// switcher.
fn heading(app: &App) -> Option<Heading> {
    let doc = &app.doc;
    let entry = doc.document.files().get(doc.document.pos.file)?;
    let commit = entry.is_commit();
    let counts = entry.counts().filter(|_| !commit);
    let staging = entry
        .change()
        .staging
        .filter(|_| {
            !commit && !matches!(entry.change().status, Status::Untracked | Status::Unmerged)
        })
        .map(crate::render::stat::staging_word);
    let path = if entry.is_page() {
        "Conversation".to_owned()
    } else {
        escape(&entry.change().path)
    };
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
    if entry.is_page() {
        return Some(Heading::File(Title {
            head: Vec::new(),
            flex: (path, Role::Plain, Cut::Right),
            name: (String::new(), Role::Plain),
            tail: Vec::new(),
        }));
    }
    if let Source::Commit(c) = &entry.source() {
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
    let flex = match &entry.change().status {
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
/// or the keys of the text box or of the thread focused or selected in the
/// comments panel, or while lines are selected, how many.
fn note(app: &App) -> Option<String> {
    app.message
        .as_ref()
        .map(|(text, _)| text.clone())
        .or_else(|| {
            if app.doc.writing.is_some() {
                return Some("esc saves · ctrl+g continues in $EDITOR".to_owned());
            }
            let panel = app.panel.as_ref().filter(|_| app.pane == Pane::Tree);
            let thread = match panel {
                Some(panel) => panel.thread().filter(|_| !panel.editing)?,
                None => app.doc.focused().filter(|_| app.doc.selection.is_none())?,
            };
            let mut keys = vec!["r reply"];
            if thread.notes.iter().any(|n| !n.posted()) {
                keys.extend(["e edit", "d delete"]);
            }
            keys.push(if thread.resolved {
                "x reopen"
            } else {
                "x resolve"
            });
            if thread.url.is_some() {
                keys.push("o open");
            }
            if thread.unposted() {
                keys.push("a make it yours");
            }
            if thread.branch.is_some() {
                keys.push("B move here");
            }
            // In the panel `esc` goes to the diff, and the thread may not be
            // in it to fold.
            if panel.is_some() {
                return Some(keys.join(" · "));
            }
            keys.push(if app.doc.fold.collapsed(&thread.root) {
                "z unfold"
            } else {
                "z fold"
            });
            keys.push("esc");
            Some(keys.join(" · "))
        })
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

/// The review's part of the status bar: `PR #7 · 18 threads · 2 drafts`,
/// the review in review mode and the counts when the comments file has
/// threads.
fn review_status(app: &App) -> Option<String> {
    let (threads, drafts) = app.comments.as_ref().map_or((0, 0), Comments::counts);
    let counts = (threads > 0).then(|| {
        let mut text = review::plural(threads, "thread");
        if drafts > 0 {
            let _ = write!(text, " · {}", review::plural(drafts, "draft"));
        }
        text
    });
    match (app.review_part(), counts) {
        (Some(review), Some(counts)) => Some(format!("{review} · {counts}")),
        (review, counts) => review.or(counts),
    }
}

/// Seconds since 1970, for the threads' ages; fixed in tests, at noon on
/// 2026-11-17 UTC, so snapshots keep their ages.
#[cfg(not(test))]
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
fn now() -> u64 {
    1_794_916_800
}

/// Whether `url` is a web link, the only kind a thread's link opens as:
/// handed to the platform's opener, anything else could start a program.
fn is_web(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
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
    let (find, other) = if tree && app.panel.is_some() {
        let search = key(Act::FilterTree).map(|k| format!("{k} search · "));
        (
            Some(format!("{}f filter", search.unwrap_or_default())),
            "diff",
        )
    } else if tree {
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
    use dffy_core::document::{self, Counts};
    use dffy_core::git::{ChangeSet, FileDiff, Mode, Repo, Side};

    use super::*;
    use crate::plugin::review::{Draft, Listed, Plan, Reason, Said, Session};
    use crate::review;
    use crate::theme::Ink;

    /// `fd` prepared as a worker does without highlighting.
    fn loaded(fd: FileDiff) -> File {
        File::prepare(crate::grammars(), None).file(fd, Whitespace::Exact, false)
    }

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
        let loaded = loaded(first);
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

    /// Each language in the list goes to the pool to compile ahead once, as
    /// its first file is listed. With highlighting off none do, until it's
    /// turned on.
    #[test]
    fn each_listed_language_is_compiled_ahead_once() {
        let names = |languages: Vec<Language>| -> Vec<String> {
            languages.iter().map(|l| l.name().to_owned()).collect()
        };
        let viewer = |config: &Config, first: Option<FileChange>| {
            App::new(
                config,
                Theme::default_theme(),
                "working tree".to_owned(),
                first,
                (100, 20),
                Indicators::Classic,
            )
        };
        let mut app = viewer(&Config::default(), Some(diff("src/main.rs").change));
        for path in [
            "src/lib.rs",
            "App.swift",
            "README",
            "Cargo.lock",
            "Sources/B.swift",
        ] {
            app.event(Event::Enumerated(diff(path).change));
        }
        assert_eq!(names(app.take_languages()), ["rust", "swift", "toml"]);
        app.event(Event::Enumerated(diff("src/c.rs").change));
        app.event(Event::Enumerated(diff("b.kt").change));
        app.event(Event::EnumerationDone(None));
        assert_eq!(names(app.take_languages()), ["kotlin"]);
        assert_eq!(names(app.take_languages()), [""; 0]);

        let off = Config {
            highlight: false,
            ..Config::default()
        };
        let mut app = viewer(&off, None);
        app.event(Event::Enumerated(diff("src/lib.rs").change));
        assert_eq!(names(app.take_languages()), [""; 0]);
        app.rediff(Act::ToggleHighlight);
        assert_eq!(names(app.take_languages()), ["rust"]);
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
        // The panel stays open after a toggle, over the rows looked at.
        press(&mut app, Key::Esc);
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
            result: Ok(Box::new(loaded(added(60)))),
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
        let current = app.doc.document.diff_of(file).map(Arc::clone);
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
            let change = app.doc.document.files()[file].change().clone();
            let fd = repo.file_diff(change, &Options::default()).expect("diff");
            Done::Diff {
                file,
                version: app.version(file),
                result: Ok(Box::new(loaded(fd))),
            }
        };
        for file in [0, 1] {
            let done = diffed(&app, file);
            app.event(Event::Diffed(done));
        }
        key(&mut app, 'f');
        assert_eq!(
            app.doc.document.files()[app.doc.document.pos.file]
                .change()
                .path,
            "b.txt"
        );
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
            app.doc.document.files()[0].wants_diff(),
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
                result: Ok(Box::new(loaded(fd))),
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
                    matches: document::find(&diff, Some(&hidden), &needle, || false),
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
        assert!(
            app.doc.document.search.is_none(),
            "typing waits for no scan"
        );
        assert!(screen(&mut app).contains("… in this file"));
        work(&mut app);
        let marks = app
            .doc
            .document
            .search
            .as_ref()
            .map(|m| m.matches.list.len());
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
        assert_eq!(app.doc.document.pos.file, 1);
        work(&mut app);
        let shown = status(&mut app);
        assert!(shown.contains("match 3/3"), "{shown}");
        key(&mut app, 'N');
        assert_eq!(app.doc.document.pos.file, 0);
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
                result: Ok(Box::new(loaded(fd))),
            }));
        }
        app.draw();
        typed(&mut app, "/needle");
        let stale_jobs = app.jobs();
        work(&mut app);
        assert_eq!(app.doc.document.search.as_ref().map(|m| m.file), Some(0));

        app.event(Event::EnumerationDone(None));
        assert_eq!(app.doc.document.files()[0].change().path, "src/a.rs");
        assert_eq!(app.doc.document.pos.file, 1, "still on src/z.rs");
        let marked = app
            .doc
            .document
            .search
            .as_ref()
            .map(|m| &app.doc.document.files()[m.file].change().path);
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
                    matches: document::find(&diff, Some(&hidden), &needle, || false),
                    diff,
                    hidden,
                }));
            }
        }
        work(&mut app);
        let marks = app
            .doc
            .document
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
        assert_eq!(app.doc.document.pos.file, 2);
        typed(&mut app, "/needle");
        app.event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
        work(&mut app);
        assert!(!status(&mut app).contains("searching this file"));
        key(&mut app, 'N');
        work(&mut app);
        assert_eq!(
            app.doc.document.pos.file, 1,
            "back to src/b.rs and its match"
        );
        let current = app.doc.document.search.as_ref().and_then(|m| m.current);
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
        assert_eq!(document::find(&diff, Some(&hidden), &needle, stale), None);
        assert!(document::find(&diff, Some(&hidden), &needle, || false).is_some());
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
        let at = app.doc.document.pos.file;
        key(&mut app, 'f');
        assert_eq!(
            app.doc.document.pos.file, at,
            "no other file passes the filter"
        );
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
            app.doc.document.files()[0].wants_diff(),
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
        assert!(
            app.doc
                .document
                .files()
                .iter()
                .all(|e| app.doc.document.is_full(e))
        );
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
            result: Ok(Box::new(loaded(first))),
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
        assert_eq!(
            (app.doc.document.pos.file, app.doc.document.pos.part),
            (1, 0)
        );
        key(&mut app, 'n');
        assert_eq!(
            (app.doc.document.pos.file, app.doc.document.pos.part),
            (1, 0)
        );
        assert!(screen(&mut app).contains("waiting for this file's diff"));
        let loaded = loaded(diff("src/zz.rs"));
        app.event(Event::Diffed(Done::Diff {
            file: 1,
            version: app.version(1),
            result: Ok(Box::new(loaded)),
        }));
        assert_eq!(app.doc.document.pos.file, 1);
        assert!(app.doc.document.pos.part > 0, "{:?}", app.doc.document.pos);
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
                    result: Ok(Box::new(loaded(late()))),
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
        assert_eq!(app.doc.document.pos.file, 1, "n moves to the next file now");
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
            .map(|(i, &(_, added, removed))| (i, app.version(i), Counts { added, removed }))
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
                let loaded = piped::read(&piped, change).expect("in memory");
                app.event(Event::Diffed(Done::Diff {
                    file,
                    version,
                    result: Ok(Box::new(loaded)),
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
        let count = |added, removed| Counts { added, removed };
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
            .map(|(i, &(_, added, removed))| (i, app.version(i), Counts { added, removed }))
            .collect();
        app.event(Event::Diffed(Done::Counts { counts }));
        app
    }

    /// The count pass starts with the files on the tree's screen, then a
    /// screen's worth below and above them, and starts again with what's on
    /// screen when the tree scrolls to files not counted yet.
    #[test]
    fn the_count_pass_starts_where_the_tree_shows() {
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "range".to_owned(),
            None,
            (200, 12),
            Indicators::Classic,
        );
        for i in 0..80 {
            app.event(Event::Enumerated(diff(&format!("f{i:02}")).change));
        }
        app.event(Event::EnumerationDone(None));
        screen(&mut app);
        app.tree.scroll_by(20);
        screen(&mut app);
        let shown: Vec<usize> = app.tree.files_on_screen().collect();
        let (top, height) = (shown[0], shown.len());
        let pass = |app: &mut App| -> Option<Vec<usize>> {
            Some(app.take_count_pass()?.iter().map(|f| f.0).collect())
        };
        let first = pass(&mut app).expect("a pass once the list is complete");
        let near: Vec<usize> = (top..top + 2 * height).chain(top - height..top).collect();
        assert_eq!(first[..near.len()], near[..]);
        assert_eq!(first.len(), 80);
        assert_eq!(pass(&mut app), None, "started again unasked");

        let counts = near
            .iter()
            .map(|&f| (f, app.version(f), Counts::default()))
            .collect();
        app.event(Event::Diffed(Done::Counts { counts }));
        app.tree.scroll_by(height.cast_signed());
        screen(&mut app);
        assert_eq!(pass(&mut app), None, "scrolled to files already counted");
        app.tree.scroll_by((2 * height).cast_signed());
        screen(&mut app);
        let shown: Vec<usize> = app.tree.files_on_screen().collect();
        let again = pass(&mut app).expect("scrolled to files not counted");
        assert_eq!(again[..shown.len()], shown[..]);
        assert_eq!(again.len(), 80 - near.len());
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
                app.doc.document.files(),
                app.doc.document.commits(),
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
        assert_eq!(app.doc.document.pos.file, 3, "kernel/sched/fair.c");
        // A tap on a row opens that file.
        key(&mut app, 'o');
        app.draw();
        app.event(Event::Input(Input::Click { x: 10, y: 1 }));
        assert!(app.picker.is_none());
        assert_eq!(app.doc.document.pos.file, 0);
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
        assert_eq!(app.doc.document.pos.file, 1);
        let [_, _, next] = hits(&app);
        tap(&mut app, next.end - 1);
        assert_eq!(app.doc.document.pos.file, 2);
        let [prev, _, _] = hits(&app);
        tap(&mut app, prev.start);
        assert_eq!(app.doc.document.pos.file, 1);
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
        let pos = app.doc.document.pos;
        app.event(Event::Input(Input::Scroll {
            delta: 1,
            x: 10,
            y: 3,
        }));
        let scrolled = screen(&mut app);
        assert_eq!(app.doc.document.pos, pos);
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
        assert_eq!(app.doc.document.pos.file, 0);
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
        let diff = Arc::clone(app.doc.document.diff_of(0).expect("diffed"));
        let rendered = view::rendered::build(Arc::clone(&diff), sides, crate::grammars(), 3, 60);
        assert_eq!(
            rendered.counts(),
            Counts {
                added: 1,
                removed: 3
            }
        );
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
        assert!(!app.doc.document.rendered(), "not on without the program");
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
            if app.doc.document.pos.offset > 0 {
                break;
            }
            key(&mut app, 'j');
        }
        assert!(
            app.doc.document.pos.offset > 0,
            "{:?}",
            app.doc.document.pos
        );
        let (pos, before) = (app.doc.document.pos, screen(&mut app));
        key(&mut app, 'j');
        assert_ne!(app.doc.document.pos, pos, "not at the bottom");
        key(&mut app, 'k');
        key(&mut app, 'f');
        assert_eq!(app.doc.document.pos.file, 1);
        key(&mut app, 'F');
        assert_eq!(app.doc.document.pos, pos);
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
        assert!(app.doc.document.structural);
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
                    result: File::prepare(crate::grammars(), None).structural(&diff),
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

    /// The screen's first `width` columns with each run of cells in one of
    /// `roles`' styles in `<role>…</>`.
    fn role_markup(app: &mut App, roles: &[Role], width: usize) -> String {
        let styles: Vec<_> = (roles.iter())
            .map(|&r| (app.theme.style(r), r.key().to_owned()))
            .collect();
        style_markup(app, &styles, width)
    }

    /// [`role_markup`] for any styles, each in `<label>…</>`.
    fn style_markup(app: &mut App, styles: &[(Style, String)], width: usize) -> String {
        app.draw();
        let grid = app.screen.shown_grid();
        (0..grid.height())
            .map(|y| {
                let mut line = String::new();
                let mut current = None;
                for cell in grid.row(y).iter().take(width) {
                    let role = styles
                        .iter()
                        .find(|(style, _)| *style == cell.style())
                        .map(|(_, label)| label.as_str());
                    if role != current {
                        if current.is_some() {
                            line.push_str("</>");
                        }
                        if let Some(label) = role {
                            line.extend(["<", label, ">"]);
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
        insta::assert_snapshot!(
            "piped_merge_commit",
            role_markup(&mut app, &roles, usize::MAX)
        );
        key(&mut app, 'f');
        read_back(&mut app);
        insta::assert_snapshot!(
            "piped_merge_combined",
            role_markup(&mut app, &roles, usize::MAX)
        );
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

    const FILE: &str = "src/some/deeply/nested/module/file.rs";

    /// A draft thread with a reply on new line 2, a posted comment on new
    /// line 50, a resolved one on removed old line 4, a comment on the
    /// whole file, one on a file not in the diff, and one on `far` whose
    /// lines are gone.
    fn review_json(far: &str) -> String {
        let line = |id: &str, path: &str, side: &str, n: usize, text: &str, extra: &str| {
            let numbers = if side == "old" {
                format!("{{\"old\":{n},\"new\":null}}")
            } else {
                format!("{{\"old\":null,\"new\":{n}}}")
            };
            format!(
                r#"{{"id":"{id}","path":"{path}","side":"{side}","start":{numbers},"end":{numbers},"text":"{text}","author":"","created":"2026-11-17T10:00:00Z","anchor":{{"before":[],"lines":["{}"],"after":[]}}{extra}}}"#,
                match id {
                    "d1" => "line 2",
                    "p1" => "line fifty",
                    "r1" => "line 4",
                    _ => "not there",
                }
            )
        };
        let comments = [
            line("d1", FILE, "new", 2, "Why two?", ""),
            r#"{"id":"d2","path":"src/some/deeply/nested/module/file.rs","text":"Because.","author":"","created":"2026-11-17T11:00:00Z","in_reply_to":"d1"}"#.to_owned(),
            line(
                "p1",
                FILE,
                "new",
                50,
                "Fifty?",
                r#","ref":"github:1","author":"octocat","url":"https://github.com/o/r/pull/1#discussion_r1","outdated":true"#,
            ),
            line("r1", FILE, "old", 4, "Old four", r#","ref":"github:2","resolved":true"#),
            r#"{"id":"f1","path":"src/some/deeply/nested/module/file.rs","text":"On the whole file","created":"2026-11-17T09:00:00Z"}"#.to_owned(),
            line("g1", "gone.rs", "new", 1, "Gone", ""),
            line("m1", far, "new", 10, "Moved away", ""),
        ];
        format!("{{\"version\":1,\"comments\":[{}]}}", comments.join(","))
    }

    /// The viewer of [`app`] with ten more files, taking comments from a
    /// file in `dir` written with [`review_json`].
    fn commented(dir: &tempfile::TempDir) -> (App, PathBuf) {
        let mut app = app(100, 20);
        for i in 0..10 {
            app.event(Event::Enumerated(diff(&format!("src/zz{i}.rs")).change));
        }
        let path = dir.path().join("comments.json");
        std::fs::write(&path, review_json("src/zz9.rs")).expect("write");
        app.set_comments(path.clone());
        (app, path)
    }

    fn read_comments(path: &std::path::Path) -> review::Review {
        review::read(path).expect("reads")
    }

    fn press(app: &mut App, key: Key) -> Option<Action> {
        app.event(Event::Input(Input::Key(key, Mods::NONE)))
    }

    /// `c` on a selection opens a text box right under it, where every key
    /// types, `enter` and a paste included; `esc` saves the text on the
    /// selected lines with their anchor and distance, and the new thread
    /// shows there, focused, with its mark in the gutter.
    #[test]
    fn c_writes_a_comment_in_a_box_under_the_selection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dffy").join("comments.json");
        let mut app = app(100, 20);
        app.set_comments(path.clone());
        screen(&mut app);
        key(&mut app, 'v');
        key(&mut app, 'j');
        assert_eq!(press(&mut app, Key::Char('c')), None);
        assert!(app.doc.selection.is_none());
        typed(&mut app, "Why these? q");
        press(&mut app, Key::Backspace);
        press(&mut app, Key::Enter);
        app.event(Event::Input(Input::Paste("Pasted\r\nlines".to_owned())));
        insta::assert_snapshot!("comment_box", screen(&mut app));
        assert!(!path.exists(), "nothing saved while writing");
        press(&mut app, Key::Esc);
        assert!(app.doc.writing.is_none());
        let saved = read_comments(&path);
        let [comment] = saved.comments.as_slice() else {
            panic!("one comment: {saved:?}");
        };
        assert_eq!(
            (comment.path.as_str(), comment.text.as_str(), comment.side),
            (FILE, "Why these? \nPasted\nlines", review::Side::New)
        );
        assert_eq!(
            (comment.start, comment.end),
            (
                Some(review::LineNumbers {
                    old: Some(0),
                    new: Some(0)
                }),
                Some(review::LineNumbers {
                    old: Some(1),
                    new: Some(1)
                })
            )
        );
        let anchor = comment.anchor.as_ref().expect("an anchor");
        assert_eq!(anchor.lines, ["line 1", "line 2"]);
        assert_eq!(anchor.after, ["line 3", "line four"]);
        assert_eq!(comment.distance, Some(2));
        let shown = screen(&mut app);
        // Outside a review session nothing is posted, so nothing's a draft.
        let rows: Vec<&str> = shown.lines().filter(|l| l.contains('●')).collect();
        assert_eq!(rows.len(), 2, "{shown}");
        assert!(rows[0].contains("line 1") && rows[1].contains("line 2"));
        let below = shown
            .lines()
            .skip_while(|l| !l.contains("line 2"))
            .nth(1)
            .unwrap_or_default();
        assert!(below.contains("▾ you · just now ━"), "{shown}");
        assert_eq!(
            app.doc.focused().map(|t| t.root.as_str()),
            Some(comment.id.as_str())
        );

        // An empty box saves nothing.
        press(&mut app, Key::Esc);
        key(&mut app, 'c');
        typed(&mut app, "  ");
        press(&mut app, Key::Esc);
        assert!(screen(&mut app).contains("empty; nothing saved"));
        assert_eq!(read_comments(&path).comments.len(), 1);

        // Editing a draft down to nothing deletes it.
        key(&mut app, 'm');
        key(&mut app, 'e');
        for _ in 0..200 {
            press(&mut app, Key::Backspace);
        }
        press(&mut app, Key::Esc);
        assert!(screen(&mut app).contains("draft deleted"));
        assert_eq!(read_comments(&path).comments.len(), 0);
    }

    /// A `d` armed for one draft doesn't outlast the mouse: after a click
    /// on another thread, `d` asks again rather than deleting it.
    #[test]
    fn a_click_disarms_d() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        let shown = screen(&mut app);
        let y = shown
            .lines()
            .position(|l| l.contains("Why two?"))
            .expect("the draft on screen");
        key(&mut app, 'm');
        assert_eq!(focused(&app), Some("f1"));
        key(&mut app, 'd');
        let y = u16::try_from(y).expect("on screen");
        app.event(Event::Input(Input::Click { x: 60, y }));
        app.event(Event::Input(Input::Release { x: 60, y }));
        assert_eq!(focused(&app), Some("d1"));
        key(&mut app, 'd');
        assert_eq!(read_comments(&path).comments.len(), 7, "nothing deleted");
        assert!(status(&mut app).contains("d again deletes"));
    }

    /// `ctrl+g` hands the box's text to the editor through the compose
    /// path; what's written there is saved. When the editor fails, the box
    /// opens again with the text. `ctrl+e`, which Ghostty sends for
    /// cmd+right, stays in the box.
    #[test]
    fn ctrl_g_continues_the_box_in_the_editor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("comments.json");
        let mut app = app(100, 20);
        app.set_comments(path.clone());
        screen(&mut app);
        key(&mut app, 'c');
        typed(&mut app, "Draft");
        let ctrl = |c| Event::Input(Input::Key(Key::Char(c), Mods::CTRL));
        assert_eq!(app.event(ctrl('e')), None);
        assert!(app.doc.writing.is_some());
        assert_eq!(
            app.event(ctrl('g')),
            Some(Action::Compose("Draft".to_owned()))
        );
        assert!(app.doc.writing.is_none());
        app.composed(None);
        let reopened = app.doc.writing.as_ref().map(|w| w.text.text().to_owned());
        assert_eq!(reopened.as_deref(), Some("Draft"));
        assert!(app.event(ctrl('g')).is_some());
        app.composed(Some("Written in vim\n".to_owned()));
        let saved = read_comments(&path);
        assert_eq!(saved.comments.len(), 1);
        assert_eq!(saved.comments[0].text, "Written in vim");
        // The command panel's entry takes the focused thread's draft there.
        key(&mut app, ' ');
        assert_eq!(
            press(&mut app, Key::Char('E')),
            Some(Action::Compose("Written in vim".to_owned()))
        );
        app.composed(Some("Edited".to_owned()));
        assert_eq!(read_comments(&path).comments[0].text, "Edited");
    }

    /// When saving the text box fails, it opens again with the text, and a
    /// second `esc` discards it rather than trying again.
    #[test]
    fn a_second_esc_discards_a_box_that_failed_to_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("file"), "").expect("write");
        let mut app = app(160, 20);
        app.set_comments(dir.path().join("file/comments.json"));
        screen(&mut app);
        key(&mut app, 'c');
        typed(&mut app, "Kept");
        press(&mut app, Key::Esc);
        assert!(app.doc.writing.is_some(), "open again");
        let shown = status(&mut app);
        assert!(shown.contains("esc again discards"), "{shown}");
        press(&mut app, Key::Esc);
        assert!(app.doc.writing.is_none());
        assert!(status(&mut app).contains("discarded"));
    }

    const BASE: &str = "aaaa111";
    const HEAD: &str = "bbbb222";

    /// A plugin that calls a review `PR #7` and posts it with four events,
    /// the first of which leaves it open on the host.
    const DESCRIBED: &str = r##"{"version":1,"command":"pr","noun":"pull request","nouns":"pull requests","short":"PR","id_prefix":"#","events":[{"id":"pending","key":"p","label":"pending: on the host, for you only","done":"review pending","submits":false},{"id":"comment","key":"c","label":"comment","done":"commented"},{"id":"approve","key":"a","label":"approve","done":"approved"},{"id":"request_changes","key":"r","label":"request changes","done":"changes requested"}]}"##;

    fn described(app: &mut App, answer: &str) {
        app.set_naming(Some(Naming::decode(answer.as_bytes()).expect("decodes")));
    }

    fn session(dir: &tempfile::TempDir, head: &str, last: Option<&str>) -> Session {
        Session {
            id: "7".to_owned(),
            label: None,
            title: Some("Spell out five".to_owned()),
            url: Some("https://github.com/acme/widgets/pull/7".to_owned()),
            base: BASE.to_owned(),
            head: head.to_owned(),
            last_reviewed_head: last.map(str::to_owned),
            state: None,
            comments: dir.path().join("comments.json"),
        }
    }

    fn said(app: &mut App, id: u64, said: Said) -> Option<Action> {
        app.event(Event::Review { id, said })
    }

    /// The run asked for: its id and arguments, and the summary it posts.
    fn plugin_run(action: Option<Action>) -> (u64, Vec<String>, Option<String>) {
        match action {
            Some(Action::Plugin {
                id, args, summary, ..
            }) => (id, args, summary),
            other => panic!("not a plugin run: {other:?}"),
        }
    }

    fn range(action: Option<Action>) -> (String, Option<PathBuf>, Option<(String, String)>) {
        match action {
            Some(Action::Retarget {
                revisions,
                comments,
                moved,
            }) => (revisions.expect("a range").revs.join(" "), comments, moved),
            other => panic!("not a retarget: {other:?}"),
        }
    }

    /// The command panel, the picker, the notes and the titles say what
    /// the review plugin reviews in its own words, and name a review by
    /// its short form, its id prefix and its label, else its id; the id
    /// is what the plugin's verbs take.
    #[test]
    fn review_labels_use_the_plugins_words() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app(160, 20);
        app.review = Some("test".to_owned());
        described(
            &mut app,
            r#"{"version":1,"command":"mr","noun":"merge request","nouns":"merge requests","short":"MR","id_prefix":"!"}"#,
        );
        let entry = app
            .command_entries()
            .into_iter()
            .find(|e| e.action == Act::OpenReview)
            .expect("the panel's entry");
        assert_eq!(entry.label, "Open merge request…");
        press(&mut app, Key::Char('p'));
        assert!(
            status(&mut app).contains("not on a merge request; dffy mr <id> opens one"),
            "{}",
            status(&mut app)
        );
        key(&mut app, ' ');
        let (list, _, _) = plugin_run(press(&mut app, Key::Char('P')));
        assert!(status(&mut app).contains("listing merge requests…"));
        said(
            &mut app,
            list,
            Said::Listed(vec![Listed {
                id: "I8f2c".to_owned(),
                label: Some("12".to_owned()),
                title: "Cache it".to_owned(),
                author: "hubot".to_owned(),
                reason: Reason::Mine,
                group: None,
                drafts: 0,
            }]),
        );
        let picker = screen(&mut app);
        assert!(
            picker.contains(" merge requests ") && picker.contains("!12 ") && !picker.contains('#'),
            "{picker}"
        );
        let (prepare, args, _) = plugin_run(press(&mut app, Key::Enter));
        assert_eq!(args, ["prepare", "I8f2c"]);
        assert!(status(&mut app).contains("fetching MR !12…"));
        let session = Session {
            id: "I8f2c".to_owned(),
            label: Some("12".to_owned()),
            ..session(&dir, HEAD, None)
        };
        said(&mut app, prepare, Said::Session(session));
        assert!(status(&mut app).contains("MR !12 · Spell out five"));
        app.message = None;
        assert!(status(&mut app).contains("MR !12 · fetching threads"));
        assert_eq!(app.description, "MR !12");
        app.job = None;
        let (_, args, _) = plugin_run(press(&mut app, Key::Char('p')));
        assert_eq!(args, ["pull", "I8f2c"]);
    }

    /// Without a description, a review reads as one by its id, in words
    /// any host's would fit.
    #[test]
    fn an_undescribed_plugin_reviews_reviews() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app(160, 20);
        app.review = Some("test".to_owned());
        press(&mut app, Key::Char('p'));
        assert!(status(&mut app).contains("not on a review"));
        let entry = (app.command_entries().into_iter())
            .find(|e| e.action == Act::OpenReview)
            .expect("the panel's entry");
        assert_eq!(entry.label, "Open review…");
        let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
        said(&mut app, prepare, Said::Progress("1/2".to_owned()));
        app.message = None;
        let shown = status(&mut app);
        assert!(shown.contains("review 7 · fetching threads 1/2"), "{shown}");
    }

    /// "Open pull request…" lists the PRs, grouped by why they're listed:
    /// the reasons dffy knows first, then the plugin's own groups in the
    /// order they came, by its titles. `enter` prepares one, whose session
    /// line switches the viewer to its range and comments in place. Its
    /// threads keep downloading, which the status bar shows. Runs replaced
    /// since say nothing.
    #[test]
    fn the_pr_picker_opens_a_pr_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app(160, 30);
        described(&mut app, DESCRIBED);
        screen(&mut app);
        key(&mut app, ' ');
        assert_eq!(press(&mut app, Key::Char('P')), None);
        assert!(status(&mut app).contains("no review plugin set"));
        app.review = Some("test".to_owned());
        key(&mut app, ' ');
        let (list, args, _) = plugin_run(press(&mut app, Key::Char('P')));
        assert_eq!(args, ["list"]);
        let listed = |id: usize, reason, group: Option<&str>| Listed {
            id: id.to_string(),
            label: None,
            title: format!("Change {id}"),
            author: "hubot".to_owned(),
            reason,
            group: group.map(str::to_owned),
            drafts: usize::from(id == 9) * 2,
        };
        said(
            &mut app,
            list,
            Said::Listed(vec![
                listed(3, Reason::Other, Some("Starred")),
                listed(11, Reason::Mine, None),
                listed(5, Reason::Other, Some("Mentions you")),
                listed(9, Reason::ReviewRequested, None),
                listed(4, Reason::Other, Some("Starred")),
                listed(7, Reason::CurrentBranch, None),
            ]),
        );
        let shown = screen(&mut app);
        let at = |text: &str| {
            shown
                .find(text)
                .unwrap_or_else(|| panic!("{text}:\n{shown}"))
        };
        assert!(at("This branch") < at("#7") && at("#7") < at("Waiting on your review"));
        assert!(at("#9") < at("Yours") && at("Yours") < at("#11"));
        assert!(at("#11") < at("Starred") && at("Starred") < at("#3"));
        assert!(at("#3") < at("#4") && at("#4") < at("Mentions you"));
        assert!(at("Mentions you") < at("#5"));
        assert!(
            at("#9") < at("2 drafts") && at("2 drafts") < at("Yours"),
            "{shown}"
        );
        press(&mut app, Key::Down);
        let (prepare, args, _) = plugin_run(press(&mut app, Key::Enter));
        assert_eq!(args, ["prepare", "9"]);
        assert!(app.reviews.is_none());
        assert!(status(&mut app).contains("fetching PR #9…"));
        assert_eq!(
            said(&mut app, list, Said::Progress("late".to_owned())),
            None
        );
        let nine = Session {
            id: "9".to_owned(),
            ..session(&dir, HEAD, None)
        };
        let (revisions, comments, _) = range(said(&mut app, prepare, Said::Session(nine)));
        assert_eq!(revisions, format!("{BASE}...{HEAD}"));
        assert_eq!(comments, Some(dir.path().join("comments.json")));
        said(&mut app, prepare, Said::Progress("100/312".to_owned()));
        app.message = None;
        assert!(
            status(&mut app).contains("PR #9 · fetching threads 100/312"),
            "{}",
            status(&mut app)
        );
        said(
            &mut app,
            prepare,
            Said::Done(Err("threads incomplete".to_owned())),
        );
        let shown = status(&mut app);
        assert!(shown.contains("PR #9: threads incomplete"), "{shown}");
        app.message = None;
        assert!(!status(&mut app).contains("fetching"));
    }

    /// The check runs once its deadline passes, never before; a moved head
    /// shows in the status bar, and `p` pulls it and shows the new range in
    /// place, through the same file of comments.
    #[test]
    fn a_due_check_finds_the_pr_moved_and_p_updates_it_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app(160, 20);
        described(&mut app, DESCRIBED);
        let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
        said(&mut app, prepare, Said::Done(Ok(())));
        let deadline = app.deadline().expect("a check is due later");
        assert!(deadline + Duration::from_secs(5) > Instant::now() + super::review::CHECK_EVERY);
        assert_eq!(app.event(Event::Tick), None);

        app.reviewing.as_mut().expect("a PR").next_check = Instant::now();
        let (check, args, _) = plugin_run(app.event(Event::Tick));
        assert_eq!(args, ["check", "7"]);
        assert_eq!(app.event(Event::Tick), None, "one check at a time");
        said(
            &mut app,
            check,
            Said::Head {
                head: HEAD.to_owned(),
                moved: false,
                comments_moved: true,
            },
        );
        said(&mut app, check, Said::Done(Ok(())));
        let shown = status(&mut app);
        assert!(
            shown.contains("PR #7 · new comments · p to update"),
            "{shown}"
        );
        app.reviewing.as_mut().expect("a PR").next_check = Instant::now();
        let (check, ..) = plugin_run(app.event(Event::Tick));
        said(
            &mut app,
            check,
            Said::Head {
                head: "cccc333".to_owned(),
                moved: true,
                comments_moved: true,
            },
        );
        said(&mut app, check, Said::Done(Ok(())));
        assert!(app.deadline().is_some_and(|at| at > Instant::now()));
        let shown = status(&mut app);
        assert!(
            shown.contains("PR #7 updated · new commits · p to update"),
            "{shown}"
        );

        let (pull, args, _) = plugin_run(press(&mut app, Key::Char('p')));
        assert_eq!(args, ["pull", "7"]);
        let (revisions, comments, moved) = range(said(
            &mut app,
            pull,
            Said::Session(session(&dir, "cccc333", None)),
        ));
        assert_eq!(revisions, format!("{BASE}...cccc333"));
        assert_eq!(comments, None, "the same comments");
        assert_eq!(moved, Some((HEAD.to_owned(), "cccc333".to_owned())));
        app.review_updated(Some(3));
        let shown = status(&mut app);
        assert!(shown.contains("PR #7 updated · 3 new commits"), "{shown}");
        said(&mut app, pull, Said::Done(Ok(())));
        app.message = None;
        assert!(!status(&mut app).contains("p to update"));

        // Up to date is said once the threads are in, not at the session
        // line, which comes before them.
        let (again, _, _) = plugin_run(press(&mut app, Key::Char('p')));
        assert_eq!(
            said(
                &mut app,
                again,
                Said::Session(session(&dir, "cccc333", None))
            ),
            None
        );
        assert!(!status(&mut app).contains("up to date"));
        said(&mut app, again, Said::Done(Ok(())));
        assert!(status(&mut app).contains("PR #7 is up to date"));
    }

    /// A warm `prepare`'s head line says the PR moved, as a check does.
    #[test]
    fn a_warm_prepare_says_the_pr_moved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app(160, 20);
        described(&mut app, DESCRIBED);
        let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
        said(
            &mut app,
            prepare,
            Said::Head {
                head: "cccc333".to_owned(),
                moved: true,
                comments_moved: false,
            },
        );
        said(&mut app, prepare, Said::Done(Ok(())));
        app.message = None;
        let shown = status(&mut app);
        assert!(
            shown.contains("PR #7 updated · new commits · p to update"),
            "{shown}"
        );
    }

    /// Without a key for a while the checks stop, waking nothing, until the
    /// next key; a check failing the same way is said once.
    #[test]
    fn checks_stop_while_idle_and_say_a_failure_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app(160, 20);
        described(&mut app, DESCRIBED);
        let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
        said(&mut app, prepare, Said::Done(Ok(())));
        let mode = app.reviewing.as_mut().expect("a PR");
        mode.next_check = Instant::now();
        mode.plans.due = None;
        app.last_input = (Instant::now().checked_sub(super::review::IDLE_AFTER))
            .and_then(|at| at.checked_sub(Duration::from_secs(1)))
            .expect("the clock is past it");
        app.message = None;
        assert_eq!(app.deadline(), None, "nothing to wake for");
        assert_eq!(app.event(Event::Tick), None);
        key(&mut app, 'j');
        let (check, args, _) = plugin_run(app.event(Event::Tick));
        assert_eq!(args, ["check", "7"]);
        said(&mut app, check, Said::Done(Err("offline".to_owned())));
        assert!(status(&mut app).contains("checking PR #7 failed: offline"));
        app.message = None;
        app.dirty = true;
        app.reviewing.as_mut().expect("a PR").next_check = Instant::now();
        let (check, ..) = plugin_run(app.event(Event::Tick));
        said(&mut app, check, Said::Done(Err("offline".to_owned())));
        assert!(
            !status(&mut app).contains("failed"),
            "said once: {}",
            status(&mut app)
        );
    }

    /// `P` shows what goes, from the plan the dialog asks for, what waits
    /// on the host, a warning when the PR moved, and the plugin's events,
    /// starting on the first that submits; a letter picks one, `s` writes
    /// the summary, `enter` posts. The status bar follows the post, and `o`
    /// opens the review it made. The summary is kept in the comments file
    /// until a submitting event sends it.
    #[test]
    fn the_post_dialog_sends_the_event_and_summary_picked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        app.resize(160, 30);
        described(&mut app, DESCRIBED);
        app.start_review("test".to_owned(), session(&dir, HEAD, None));
        app.job = None;
        assert_eq!(path, dir.path().join("comments.json"), "the session's");
        app.reviewing.as_mut().expect("a PR").moved = Some("cccc333".to_owned());
        let (plan, args, _) = plugin_run(press(&mut app, Key::Char('P')));
        assert_eq!(args, ["post", "7", "--dry-run"]);
        let shown = screen(&mut app);
        assert!(shown.contains("finish your review · PR #7"), "{shown}");
        assert!(
            shown.contains("5 comments") && shown.contains("checking what can be posted…"),
            "{shown}"
        );
        said(
            &mut app,
            plan,
            Said::Planned {
                line: 5,
                file: 0,
                skip: 0,
                pending: 2,
                resolves: 1,
            },
        );
        said(&mut app, plan, Said::Done(Ok(())));
        let shown = screen(&mut app);
        assert!(
            shown.contains("5 comments · 1 resolve change · 2 pending on PR #7"),
            "{shown}"
        );
        assert!(!shown.contains("checking"), "{shown}");
        assert!(shown.contains("PR #7 has new commits"), "{shown}");
        assert!(
            shown.contains("(•) c  comment") && shown.contains("( ) r  request changes"),
            "the plugin's events, the first that submits picked: {shown}"
        );
        key(&mut app, 'a');
        key(&mut app, 's');
        typed(&mut app, "Ship it");
        press(&mut app, Key::Esc);
        assert!(app.posting.is_some(), "esc leaves the summary first");
        let (post, args, summary) = plugin_run(press(&mut app, Key::Enter));
        assert_eq!(args, ["post", "7", "--event", "approve"]);
        assert_eq!(summary.as_deref(), Some("Ship it"));
        assert_eq!(read_comments(&path).summary.as_deref(), Some("Ship it"));
        said(&mut app, post, Said::Progress("3/6".to_owned()));
        app.message = None;
        assert!(status(&mut app).contains("PR #7 · posting 3/6"));
        assert_eq!(press(&mut app, Key::Char('p')), None, "no update meanwhile");
        let url = "https://github.com/acme/widgets/pull/7#pullrequestreview-1";
        said(
            &mut app,
            post,
            Said::Posted {
                posted: 5,
                failed: 0,
                pending: 2,
                url: Some(url.to_owned()),
            },
        );
        said(&mut app, post, Said::Done(Ok(())));
        let shown = status(&mut app);
        assert!(shown.contains(&format!("review sent · {url}")), "{shown}");
        assert_eq!(
            press(&mut app, Key::Char('o')),
            Some(Action::Browse(url.to_owned()))
        );
        let mode = app.reviewing.as_ref().expect("a PR");
        assert_eq!(mode.session.last_reviewed_head.as_deref(), Some(HEAD));
        assert_eq!(read_comments(&path).summary, None, "sent");
        // The next opening starts on the first event that submits again;
        // a plan that fails says so where the counts' check would.
        app.job = None;
        let (plan, ..) = plugin_run(press(&mut app, Key::Char('P')));
        let shown = screen(&mut app);
        assert!(shown.contains("(•) c  comment"), "{shown}");
        said(&mut app, plan, Said::Done(Err("no session".to_owned())));
        let shown = screen(&mut app);
        assert!(
            shown.contains("couldn't check what can be posted: no session"),
            "{shown}"
        );
    }

    /// A plugin whose events all leave the review open starts the dialog on
    /// the one last sent.
    #[test]
    fn without_a_submitting_event_the_dialog_starts_on_the_last_sent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        app.resize(160, 30);
        described(
            &mut app,
            r#"{"version":1,"command":"cr","noun":"change","nouns":"changes","events":[{"id":"draft","key":"d","label":"keep as draft","done":"kept","submits":false},{"id":"share","key":"h","label":"share","done":"shared","submits":false}]}"#,
        );
        app.start_review("test".to_owned(), session(&dir, HEAD, None));
        app.job = None;
        press(&mut app, Key::Char('P'));
        assert!(screen(&mut app).contains("(•) d  keep as draft"));
        key(&mut app, 'h');
        let (post, ..) = plugin_run(press(&mut app, Key::Enter));
        said(&mut app, post, Said::Done(Ok(())));
        press(&mut app, Key::Char('P'));
        assert!(screen(&mut app).contains("(•) h  share"));
    }

    /// "Suggest change" left as it opened, the lines as they are and
    /// nothing said, saves nothing.
    #[test]
    fn an_untouched_suggestion_saves_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("comments.json");
        let mut app = app(100, 30);
        app.set_comments(path.clone());
        screen(&mut app);
        key(&mut app, 'S');
        press(&mut app, Key::Esc);
        assert!(!path.exists() || read_comments(&path).comments.is_empty());
    }

    /// The summary outlasts the dialog, in the comments file, and comes
    /// back with it; an event that leaves the review open keeps it.
    #[test]
    fn the_summary_is_kept_until_a_review_sends_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        app.resize(160, 30);
        described(&mut app, DESCRIBED);
        app.start_review("test".to_owned(), session(&dir, HEAD, None));
        app.job = None;
        press(&mut app, Key::Char('P'));
        key(&mut app, 's');
        typed(&mut app, "Two nits");
        press(&mut app, Key::Esc);
        press(&mut app, Key::Esc);
        assert!(app.posting.is_none());
        assert_eq!(read_comments(&path).summary.as_deref(), Some("Two nits"));
        press(&mut app, Key::Char('P'));
        assert!(screen(&mut app).contains("Two nits"));
        key(&mut app, 'p');
        let (post, ..) = plugin_run(press(&mut app, Key::Enter));
        said(
            &mut app,
            post,
            Said::Posted {
                posted: 5,
                failed: 0,
                pending: 0,
                url: None,
            },
        );
        said(&mut app, post, Said::Done(Ok(())));
        assert_eq!(read_comments(&path).summary.as_deref(), Some("Two nits"));
    }

    /// Comments in your pending review on the host show `pending`, and the
    /// status bar says how many wait and that `P` submits them; with no
    /// draft, an event that submits still goes.
    #[test]
    fn pending_comments_show_and_p_submits_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = r#"{"version":1,"comments":[
{"id":"p1","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":null,"new":4},"end":{"old":null,"new":4},"text":"Waiting","author":"","created":"2026-11-17T10:00:00Z","ref":"github:1","pending":true,"anchor":{"before":[],"lines":["line four"],"after":[]}}
]}"#;
        let mut app = reviewed(&dir, json, (160, 30));
        described(&mut app, DESCRIBED);
        app.job = None;
        app.message = None;
        let shown = screen(&mut app);
        assert!(shown.contains("you · 2h ago · pending"), "{shown}");
        assert!(
            status(&mut app).contains("review 7 · 1 pending · P submits"),
            "{}",
            status(&mut app)
        );
        let (plan, ..) = plugin_run(press(&mut app, Key::Char('P')));
        said(
            &mut app,
            plan,
            Said::Planned {
                line: 0,
                file: 0,
                skip: 0,
                pending: 1,
                resolves: 0,
            },
        );
        said(&mut app, plan, Said::Done(Ok(())));
        assert!(screen(&mut app).contains("1 pending on review 7"));
        key(&mut app, 'a');
        let (_, args, _) = plugin_run(press(&mut app, Key::Enter));
        assert_eq!(args, ["post", "7", "--event", "approve"]);
    }

    /// The local review's file: a draft thread on new line 2 with a reply,
    /// a draft whose line isn't in the PR's diff, and a posted comment.
    const LOCAL: &str = r#"{"version":1,"comments":[
{"id":"l1","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":2,"new":2},"end":{"old":2,"new":2},"text":"Local one","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":["line 1"],"lines":["line 2"],"after":["line 3"]}},
{"id":"l2","path":"src/some/deeply/nested/module/file.rs","text":"Local reply","author":"","created":"2026-11-17T10:05:00Z","in_reply_to":"l1"},
{"id":"l9","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":null,"new":30},"end":{"old":null,"new":30},"text":"Not on this diff","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":[],"lines":["only in the working tree"],"after":[]}},
{"id":"lp","path":"src/some/deeply/nested/module/file.rs","text":"Posted from here","author":"","created":"2026-11-17T10:00:00Z","ref":"github:9"}
]}"#;

    /// The PR's file: your draft on new line 4, and a posted comment on
    /// new line 50.
    const PR_FILE: &str = r#"{"version":1,"comments":[
{"id":"pd","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":null,"new":4},"end":{"old":null,"new":4},"text":"PR draft","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":[],"lines":["line four"],"after":[]}},
{"id":"pp","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":null,"new":50},"end":{"old":null,"new":50},"text":"Fifty?","author":"octocat","created":"2026-11-17T10:00:00Z","ref":"github:1","anchor":{"before":[],"lines":["line fifty"],"after":[]}}
]}"#;

    /// A viewer on PR #7 with [`PR_FILE`], plain dffy's [`LOCAL`] beside it.
    /// Returns the PR's file and the local one.
    fn pr_with_local(dir: &tempfile::TempDir) -> (App, PathBuf, PathBuf) {
        let (pr, local) = (
            dir.path().join("comments.json"),
            dir.path().join("local.json"),
        );
        std::fs::write(&pr, PR_FILE).expect("write");
        std::fs::write(&local, LOCAL).expect("write");
        let mut app = app(100, 30);
        described(&mut app, DESCRIBED);
        app.set_local_comments(Some(local.clone()));
        app.start_review("test".to_owned(), session(dir, HEAD, None));
        app.job = None;
        app.set_comments(pr.clone());
        (app, pr, local)
    }

    /// On a PR, the local review's drafts that place on its diff show with
    /// a `local` chip and count as drafts; one that doesn't place stays out
    /// of view and of the `i` list, and a posted one isn't brought. Edits
    /// and replies in a local thread go to the local file, a new comment to
    /// the PR's.
    #[test]
    fn local_drafts_show_on_a_pr_and_are_written_where_they_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, pr, local) = pr_with_local(&dir);
        let shown = screen(&mut app);
        assert!(shown.contains("draft · local"), "{shown}");
        assert!(shown.contains("Local reply"), "{shown}");
        assert!(!shown.contains("Not on this diff") && !shown.contains("Posted from here"));
        assert_eq!(
            app.sending(true),
            super::review::Sending {
                comments: 3,
                local: 2,
                ..super::review::Sending::default()
            }
        );
        key(&mut app, 'i');
        let list = screen(&mut app);
        assert!(
            list.contains("Local one") && !list.contains("Not on this diff"),
            "{list}"
        );
        press(&mut app, Key::Esc);

        let pr_before = std::fs::read(&pr).expect("read");
        app.follow_thread("l1");
        // `e` takes the thread's last draft: the reply.
        key(&mut app, 'e');
        typed(&mut app, " more");
        press(&mut app, Key::Esc);
        key(&mut app, 'r');
        typed(&mut app, "Again");
        press(&mut app, Key::Esc);
        let replies: Vec<_> = (read_comments(&local).comments.into_iter())
            .filter(|c| c.in_reply_to.as_deref() == Some("l1"))
            .map(|c| c.text)
            .collect();
        assert_eq!(replies, ["Local reply more", "Again"]);
        assert_eq!(std::fs::read(&pr).expect("read"), pr_before, "not the PR's");
        press(&mut app, Key::Esc);
        key(&mut app, 'c');
        typed(&mut app, "New here");
        press(&mut app, Key::Esc);
        let texts: Vec<String> = read_comments(&pr)
            .comments
            .into_iter()
            .map(|c| c.text)
            .collect();
        assert_eq!(texts, ["PR draft", "Fifty?", "New here"]);
        assert_eq!(read_comments(&local).comments.len(), 5);
    }

    /// The post dialog plans the local drafts in view with the review's,
    /// through `--drafts`, and sends them only with `l`: those written
    /// before the session started aren't sent unasked. Sending moves them
    /// into the PR's file, with new ids, before `post` runs, and out of the
    /// local file after; when the PR's file can't take them, nothing is
    /// posted, the dialog stays and both files are as they were.
    #[test]
    fn posting_moves_the_local_drafts_into_the_prs_file_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, pr, local) = pr_with_local(&dir);
        let (check, args, _) = plugin_run(press(&mut app, Key::Char('P')));
        assert_eq!(
            args,
            [
                "post",
                "7",
                "--dry-run",
                "--drafts",
                local.to_str().expect("utf-8")
            ]
        );
        said(&mut app, check, draft("l1", Plan::File));
        said(
            &mut app,
            check,
            Said::Planned {
                line: 2,
                file: 1,
                skip: 0,
                pending: 0,
                resolves: 0,
            },
        );
        said(&mut app, check, Said::Done(Ok(())));
        let shown = screen(&mut app);
        assert!(
            shown.contains("1 comment") && shown.contains("l  [ ] with the 2 local drafts"),
            "written before the session: {shown}"
        );
        key(&mut app, 'l');
        let shown = screen(&mut app);
        assert!(
            shown.contains("3 comments · 1 as a file comment")
                && shown.contains("l  [x] with the 2 local drafts"),
            "{shown}"
        );

        std::fs::write(&pr, r#"{"version":2,"comments":[]}"#).expect("write");
        let local_before = std::fs::read(&local).expect("read");
        assert_eq!(press(&mut app, Key::Enter), None);
        assert!(app.posting.is_some(), "the dialog stays");
        assert!(status(&mut app).contains("nothing posted"));
        assert_eq!(std::fs::read(&local).expect("read"), local_before);
        assert_eq!(
            std::fs::read_to_string(&pr).expect("read"),
            r#"{"version":2,"comments":[]}"#
        );

        std::fs::write(&pr, PR_FILE).expect("write");
        let (_, args, _) = plugin_run(press(&mut app, Key::Enter));
        assert_eq!(args, ["post", "7", "--event", "comment"]);
        let left: Vec<String> = read_comments(&local)
            .comments
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(left, ["l9", "lp"]);
        let moved = read_comments(&pr).comments;
        let texts: Vec<&str> = moved.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["PR draft", "Fifty?", "Local one", "Local reply"]);
        assert!(moved[2].id != "l1" && moved[2].reference.is_none());
        assert_eq!(moved[3].in_reply_to.as_ref(), Some(&moved[2].id));
        let shown = screen(&mut app);
        assert!(
            shown.contains("Local one") && !shown.contains("· local"),
            "{shown}"
        );
    }

    fn draft(id: &str, plan: Plan) -> Said {
        Said::Draft(Draft {
            id: id.to_owned(),
            path: FILE.to_owned(),
            plan,
        })
    }

    fn skip(why: &str) -> Plan {
        Plan::Skip(why.to_owned())
    }

    fn focused(app: &App) -> Option<&str> {
        app.doc.focused().map(|t| t.root.as_str())
    }

    /// What the heading of [`FILE`]'s comment `id` says posting it does.
    fn posting(app: &App, id: &str) -> Option<(Role, String)> {
        let threads = app.comments.as_ref()?.threads(FILE.as_bytes())?;
        let note = threads.iter().flat_map(|t| &t.notes).find(|n| n.id == id)?;
        note.posting.clone()
    }

    /// The dialog asks for a plan beside the run going on and counts by
    /// it, with one warning for the drafts that won't go: `v` closes the
    /// dialog on the first, its thread focused, the summary kept; `m` goes
    /// on through the threads from there. Each draft's heading says what
    /// posting it does.
    #[test]
    fn the_post_dialog_warns_of_drafts_that_wont_go_and_v_goes_there() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        app.resize(160, 30);
        described(&mut app, DESCRIBED);
        let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
        screen(&mut app);
        let (check, args, _) = plugin_run(press(&mut app, Key::Char('P')));
        assert_eq!(args, ["post", "7", "--dry-run"]);
        assert!(
            app.job.as_ref().is_some_and(|r| r.id == prepare),
            "the prepare goes on"
        );
        said(&mut app, check, draft("d1", Plan::Line));
        said(
            &mut app,
            check,
            draft("d2", skip("its thread isn't posted")),
        );
        said(&mut app, check, draft("f1", skip("file not in the PR")));
        said(&mut app, check, draft("m1", Plan::File));
        said(&mut app, check, Said::Done(Ok(())));
        let shown = screen(&mut app);
        assert!(
            shown.contains("5 comments · 1 as a file comment"),
            "{shown}"
        );
        assert!(
            shown.contains("! 2 drafts wait for a push · v goes to them"),
            "{shown}"
        );
        key(&mut app, 's');
        typed(&mut app, "Later");
        press(&mut app, Key::Esc);

        key(&mut app, 'v');
        assert!(app.posting.is_none());
        assert_eq!(read_comments(&path).summary.as_deref(), Some("Later"));
        assert_eq!(focused(&app), Some("d1"), "the reply's thread");
        let shown = status(&mut app);
        assert!(
            shown.contains("not posted: its thread isn't posted · 1 more · m/M go between threads"),
            "{shown}"
        );
        let shown = screen(&mut app);
        assert!(
            shown.contains("Because.") && shown.contains("draft · not in the PR yet"),
            "{shown}"
        );
        assert_eq!(posting(&app, "d1"), None, "posts on its line");
        assert_eq!(
            posting(&app, "f1"),
            Some((Role::Warn, "not in the PR yet".to_owned()))
        );
    }

    /// A draft the host refused before says why in its heading, and the
    /// dialog warns of it; a post that leaves drafts says how many were
    /// refused, and the review is sent without them.
    #[test]
    fn refused_drafts_say_why_and_the_post_says_how_it_went() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        app.resize(160, 30);
        described(&mut app, DESCRIBED);
        app.start_review("test".to_owned(), session(&dir, HEAD, None));
        app.job = None;
        let refuse = |path: &PathBuf, why: &str| {
            review::apply(
                path,
                vec![Op::Set {
                    target: Target::Id("d1".to_owned()),
                    change: Change {
                        post_error: Some(Some(why.to_owned())),
                        ..Change::default()
                    },
                }],
            )
            .expect("applies");
        };
        refuse(&path, "Line could not be resolved");
        app.event(Event::Comments);
        let shown = screen(&mut app);
        assert!(shown.contains("draft · refused: Line could"), "{shown}");
        let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
        said(&mut app, check, Said::Done(Ok(())));
        assert!(screen(&mut app).contains("! 1 refused before · v goes to them"));
        key(&mut app, 'a');
        let (post, ..) = plugin_run(press(&mut app, Key::Enter));
        said(
            &mut app,
            post,
            draft("d1", skip("Line could not be resolved")),
        );
        said(&mut app, post, draft("g1", skip("file not in the PR")));
        said(
            &mut app,
            post,
            Said::Posted {
                posted: 3,
                failed: 2,
                pending: 0,
                url: None,
            },
        );
        refuse(&path, "Line could not be resolved");
        said(&mut app, post, Said::Done(Ok(())));
        let shown = status(&mut app);
        assert!(
            shown.contains(
                "review sent · 1 comment refused, kept as drafts · 1 draft waits for a push"
            ),
            "{shown}"
        );
    }

    /// An event that leaves the review open on the host says it in its own
    /// words; with nothing posted, it says so.
    #[test]
    fn an_event_that_leaves_the_review_open_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        app.resize(160, 30);
        described(&mut app, DESCRIBED);
        app.start_review("test".to_owned(), session(&dir, HEAD, None));
        app.job = None;
        for (posted, failed, said_after) in [
            (2, 1, "review pending · 1 draft waits for a push"),
            (0, 0, "nothing posted"),
        ] {
            if let Some((check, ..)) = press(&mut app, Key::Char('P')).map(|a| plugin_run(Some(a)))
            {
                said(&mut app, check, Said::Done(Ok(())));
            }
            key(&mut app, 'p');
            let (post, args, _) = plugin_run(press(&mut app, Key::Enter));
            assert_eq!(args, ["post", "7", "--event", "pending"]);
            if failed > 0 {
                said(&mut app, post, draft("g1", skip("file not in the PR")));
            }
            said(
                &mut app,
                post,
                Said::Posted {
                    posted,
                    failed,
                    pending: 0,
                    url: None,
                },
            );
            said(&mut app, post, Said::Done(Ok(())));
            let shown = status(&mut app);
            assert!(shown.contains(said_after), "{shown}");
        }
    }

    /// A plugin that names no events posts with one, `post`.
    #[test]
    fn without_events_a_review_is_posted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        app.resize(160, 30);
        described(
            &mut app,
            r#"{"version":1,"command":"cr","noun":"change","nouns":"changes"}"#,
        );
        app.start_review("test".to_owned(), session(&dir, HEAD, None));
        app.job = None;
        let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
        said(&mut app, check, Said::Done(Ok(())));
        let shown = screen(&mut app);
        assert!(shown.contains("(•) p  post"), "{shown}");
        let (post, args, _) = plugin_run(press(&mut app, Key::Enter));
        assert_eq!(args, ["post", "7", "--event", "post"]);
        said(
            &mut app,
            post,
            Said::Posted {
                posted: 5,
                failed: 0,
                pending: 0,
                url: None,
            },
        );
        said(&mut app, post, Said::Done(Ok(())));
        assert!(status(&mut app).contains("change 7 · review sent"));
    }

    /// While a post runs, the first quit says how far it got and that
    /// another stops it; the second quits. After the post, one quit does.
    #[test]
    fn a_quit_while_posting_asks_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        app.resize(160, 30);
        described(&mut app, DESCRIBED);
        app.start_review("test".to_owned(), session(&dir, HEAD, None));
        app.job = None;
        let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
        said(&mut app, check, Said::Done(Ok(())));
        let (post, ..) = plugin_run(press(&mut app, Key::Enter));
        said(&mut app, post, Said::Progress("37/200".to_owned()));
        assert_eq!(press(&mut app, Key::Char('q')), None);
        assert!(status(&mut app).contains("still posting 37/200 · q again stops it"));
        assert_eq!(press(&mut app, Key::Char('q')), Some(Action::Quit));
        said(&mut app, post, Said::Done(Ok(())));
        assert_eq!(press(&mut app, Key::Char('q')), Some(Action::Quit));
    }

    /// The drafts are planned in the background a moment after they
    /// change, never per frame, and the plan's words go in their
    /// headings; a plan that fails says nothing.
    #[test]
    fn drafts_are_planned_in_the_background_after_they_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        app.resize(160, 30);
        described(&mut app, DESCRIBED);
        app.start_review("test".to_owned(), session(&dir, HEAD, None));
        app.job = None;
        let due = app.deadline().expect("a plan is due");
        assert!(due <= Instant::now() + Duration::from_secs(1), "soon");
        app.reviewing.as_mut().expect("a PR").plans.due = Some(Instant::now());
        let (plan, args, _) = plugin_run(app.event(Event::Tick));
        assert_eq!(args, ["post", "7", "--dry-run"]);
        assert_eq!(app.event(Event::Tick), None, "one at a time");
        said(&mut app, plan, draft("d1", Plan::File));
        said(&mut app, plan, Said::Done(Ok(())));
        let file_comment = Some((Role::Dim, "posts as file comment".to_owned()));
        assert_eq!(posting(&app, "d1"), file_comment);
        assert_eq!(app.event(Event::Tick), None, "nothing changed since");
        app.follow_thread("d1");

        key(&mut app, 'r');
        typed(&mut app, "More");
        press(&mut app, Key::Esc);
        app.reviewing.as_mut().expect("a PR").plans.due = Some(Instant::now());
        let (plan, ..) = plugin_run(app.event(Event::Tick));
        app.message = None;
        said(&mut app, plan, Said::Done(Err("nope".to_owned())));
        assert!(!status(&mut app).contains("nope"), "{}", status(&mut app));
        assert_eq!(posting(&app, "d1"), file_comment, "the last plan stays");
    }

    /// The plugin's notice, written as its post ends, is what the status
    /// bar says after it, over dffy's own words for how it went, which the
    /// review's part keeps; whether the watcher read it first or the
    /// run's end did.
    #[test]
    fn the_plugins_notice_outlasts_the_post() {
        let url = "https://example.com/review/7";
        for watched in [false, true] {
            let dir = tempfile::tempdir().expect("tempdir");
            let (mut app, path) = commented(&dir);
            app.resize(160, 30);
            described(&mut app, DESCRIBED);
            app.start_review("test".to_owned(), session(&dir, HEAD, None));
            app.job = None;
            let (check, ..) = plugin_run(press(&mut app, Key::Char('P')));
            said(&mut app, check, Said::Done(Ok(())));
            key(&mut app, 'c');
            let (post, ..) = plugin_run(press(&mut app, Key::Enter));
            said(
                &mut app,
                post,
                Said::Posted {
                    posted: 3,
                    failed: 0,
                    pending: 0,
                    url: Some(url.to_owned()),
                },
            );
            review::apply(
                &path,
                vec![Op::Notice(Some("a resolve was refused".to_owned()))],
            )
            .expect("applies");
            if watched {
                app.event(Event::Comments);
            }
            said(&mut app, post, Said::Done(Ok(())));
            let shown = status(&mut app);
            assert!(
                shown.contains("a resolve was refused"),
                "{watched}: {shown}"
            );
            assert!(shown.contains("PR #7 · review sent"), "{watched}: {shown}");
        }
    }

    /// "Since last review" switches to the range from the head last
    /// reviewed and back; without one it says so.
    #[test]
    fn since_last_review_toggles_the_range() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app(160, 20);
        described(&mut app, DESCRIBED);
        app.start_review("test".to_owned(), session(&dir, HEAD, None));
        key(&mut app, ' ');
        assert_eq!(press(&mut app, Key::Char('R')), None);
        assert!(status(&mut app).contains("no review of yours posted"));
        app.reviewing
            .as_mut()
            .expect("a PR")
            .session
            .last_reviewed_head = Some("9999aaa".to_owned());
        key(&mut app, ' ');
        let (revisions, ..) = range(press(&mut app, Key::Char('R')));
        assert_eq!(revisions, format!("9999aaa..{HEAD}"));
        assert!(status(&mut app).contains("since your last review: 9999aaa..bbbb222"));
        assert_eq!(app.description, "PR #7 since your review");
        key(&mut app, ' ');
        let (revisions, ..) = range(press(&mut app, Key::Char('R')));
        assert_eq!(revisions, format!("{BASE}...{HEAD}"));
    }

    /// The branch's PR, found by `detect`, is prepared and brings its
    /// comments to plain dffy's range, which stays; "Show whole pull
    /// request" shows its base against the working tree, and goes back.
    /// With no PR, nothing changes.
    #[test]
    fn a_detected_pr_keeps_the_range_and_adds_its_comments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app(160, 20);
        described(&mut app, DESCRIBED);
        let plain = Revisions {
            unstaged: true,
            ..Revisions::default()
        };
        app.set_selected(plain.clone(), DefaultDiff::Unstaged);
        let title = app.description.clone();
        let Action::Plugin { id, args, .. } = app.detect_review("test".to_owned()) else {
            panic!("a run");
        };
        assert_eq!(args, ["detect"]);
        let (prepare, args, _) = plugin_run(said(
            &mut app,
            id,
            Said::Detected(Some(session(&dir, HEAD, None))),
        ));
        assert_eq!(args, ["prepare", "7"]);
        said(&mut app, id, Said::Done(Ok(())));
        match said(&mut app, prepare, Said::Session(session(&dir, HEAD, None))) {
            Some(Action::Retarget {
                revisions,
                comments,
                ..
            }) => {
                assert_eq!(revisions, None, "the range stays");
                assert_eq!(comments, Some(dir.path().join("comments.json")));
            }
            other => panic!("not a retarget: {other:?}"),
        }
        assert_eq!(app.description, title);
        let whole = |app: &mut App| {
            key(app, ' ');
            match press(app, Key::Char('A')) {
                Some(Action::Retarget {
                    revisions: Some(revisions),
                    comments: None,
                    ..
                }) => revisions,
                other => panic!("not a retarget: {other:?}"),
            }
        };
        let revisions = whole(&mut app);
        assert_eq!(revisions.revs, [BASE]);
        assert!(revisions.merge_base && !revisions.staged);
        assert_eq!(app.description, "PR #7 + working tree");
        assert_eq!(app.whole_review_title(), "Show working tree changes");
        assert_eq!(whole(&mut app), plain);
        assert_eq!(app.description, title);
        assert_eq!(app.whole_review_title(), "Show whole pull request");

        let mut app = self::app(160, 20);
        let Action::Plugin { id, .. } = app.detect_review("test".to_owned()) else {
            panic!("a run");
        };
        assert_eq!(said(&mut app, id, Said::Detected(None)), None);
        said(&mut app, id, Said::Done(Ok(())));
        assert!(app.reviewing.is_none() && app.job.is_none());
    }

    /// Listing PRs runs beside a `prepare`, which keeps saying how far it
    /// got.
    #[test]
    fn listing_prs_leaves_the_run_going_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app(160, 20);
        described(&mut app, DESCRIBED);
        let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
        key(&mut app, ' ');
        let (list, args, _) = plugin_run(press(&mut app, Key::Char('P')));
        assert_eq!(args, ["list"]);
        said(&mut app, prepare, Said::Progress("2/9".to_owned()));
        said(&mut app, list, Said::Done(Ok(())));
        app.message = None;
        let shown = status(&mut app);
        assert!(shown.contains("PR #7 · fetching threads 2/9"), "{shown}");
    }

    #[test]
    fn review_keys_refuse_a_piped_diff_and_need_a_pr() {
        let mut app = app(100, 20);
        assert_eq!(press(&mut app, Key::Char('p')), None);
        assert!(screen(&mut app).contains("not on a review"));
        app.set_piped();
        app.review = Some("test".to_owned());
        key(&mut app, ' ');
        assert_eq!(press(&mut app, Key::Char('P')), None);
        assert!(screen(&mut app).contains("a piped diff can't be reviewed"));
    }

    /// Threads show under the lines they're on, each comment with its
    /// author, age and state, replies indented; the comment on the whole
    /// file under its first row. The gutter keeps a mark per thread, the
    /// tree counts each file's comments, and the status bar counts the
    /// threads, and in a review session the drafts.
    #[test]
    fn threads_show_inline_under_their_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        insta::assert_snapshot!("comment_marks", screen(&mut app));
        app.resize(160, 30);
        let wide = screen(&mut app);
        assert!(wide.contains("6 threads ─"), "{wide}");
        let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
        said(&mut app, prepare, Said::Done(Ok(())));
        let wide = screen(&mut app);
        assert!(wide.contains("6 threads · 5 drafts"), "{wide}");
    }

    /// Files with comments beyond the ones on screen and prefetched are
    /// diffed so their threads get placed; until then the comments panel
    /// says it's still looking, and it lists the far file's threads once
    /// they are, those on a file not in the diff included.
    #[test]
    fn far_files_with_comments_are_diffed_and_every_thread_listed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        screen(&mut app);
        let far = 11;
        assert_eq!(app.doc.document.files()[far].change().path, "src/zz9.rs");
        assert!(
            app.jobs()
                .iter()
                .any(|j| matches!(j, Job::Diff { file, .. } if *file == far)),
            "the far file is diffed"
        );
        key(&mut app, 'i');
        let listed = left(&mut app);
        assert!(listed.contains("finding the lines"), "{listed}");
        assert!(!listed.contains("Moved"), "{listed}");
        let loaded = loaded(diff("src/zz9.rs"));
        app.event(Event::Diffed(Done::Diff {
            file: far,
            version: app.version(far),
            result: Ok(Box::new(loaded)),
        }));
        assert!(
            !app.jobs()
                .iter()
                .any(|j| matches!(j, Job::Diff { file, .. } if *file == far)),
            "placed, it isn't diffed again"
        );
        let listed = left(&mut app);
        assert!(
            listed.contains("Moved") && listed.contains("Gone"),
            "{listed}"
        );
    }

    /// [`commented`] with `json` for its comments, in a review session so
    /// your comments are drafts, every file placed, `width` × `height`.
    fn reviewed(dir: &tempfile::TempDir, json: &str, (width, height): (usize, usize)) -> App {
        let (mut app, path) = commented(dir);
        std::fs::write(&path, json).expect("write");
        let prepare = app.start_review("test".to_owned(), session(dir, HEAD, None));
        said(&mut app, prepare, Said::Done(Ok(())));
        app.resize(width, height);
        screen(&mut app);
        let far = 11;
        app.event(Event::Diffed(Done::Diff {
            file: far,
            version: app.version(far),
            result: Ok(Box::new(loaded(diff("src/zz9.rs")))),
        }));
        app
    }

    /// [`reviewed`] with the comments in the left panel.
    fn listed(dir: &tempfile::TempDir, json: &str, size: (usize, usize)) -> App {
        let mut app = reviewed(dir, json, size);
        key(&mut app, 'i');
        app
    }

    /// The selected thread's root in the comments panel.
    fn selected(app: &App) -> String {
        let panel = app.panel.as_ref().expect("the comments show");
        panel.selected().unwrap_or_default().to_owned()
    }

    /// The left panel's columns of the screen.
    fn left(app: &mut App) -> String {
        let shown = screen(app);
        let width = app.tree_columns().unwrap_or(0);
        (shown.lines())
            .map(|l| l.chars().take(width).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The file shown and the root of the thread focused in it.
    fn at(app: &App) -> (usize, String) {
        let focused = app.doc.focused().map(|t| t.root.clone());
        (app.doc.document.pos.file, focused.unwrap_or_default())
    }

    /// `json` with general comments, on the review rather than a file, as
    /// a review plugin imports them: the description last in the file and
    /// the others out of date order.
    fn with_conversation(json: &str) -> String {
        let general = [
            r#"{"id":"c2","text":"Is this ready?","author":"hubot","created":"2026-11-16T09:00:00Z","ref":"h:2","kind":"comment"}"#,
            r#"{"id":"r1","text":"Looks good, one question inline.","author":"octocat","created":"2026-11-16T12:00:00Z","ref":"h:3","kind":"review","label":"approved","url":"https://example.com/7#review-1"}"#,
            r#"{"id":"c1","text":"Thanks for looking.","author":"","created":"2026-11-15T10:00:00Z","ref":"h:1","kind":"comment"}"#,
            r#"{"id":"d1","text":"Spells out five.\n\nThe count was off by one.","author":"ana","created":"2026-11-15T12:00:00Z","ref":"h:0","kind":"description"}"#,
        ];
        json.replacen(
            r#""comments":["#,
            &format!(r#""comments":[{},"#, general.join(",")),
            1,
        )
    }

    /// [`reviewed`] with general comments too.
    fn conversing(dir: &tempfile::TempDir, (width, height): (usize, usize)) -> (App, PathBuf) {
        let (mut app, path) = commented(dir);
        std::fs::write(&path, with_conversation(&review_json("src/zz9.rs"))).expect("write");
        let prepare = app.start_review("test".to_owned(), session(dir, HEAD, None));
        said(&mut app, prepare, Said::Done(Ok(())));
        app.resize(width, height);
        screen(&mut app);
        // After the conversation, first.
        let far = 12;
        assert_eq!(app.doc.document.files()[far].change().path, "src/zz9.rs");
        app.event(Event::Diffed(Done::Diff {
            file: far,
            version: app.version(far),
            result: Ok(Box::new(loaded(diff("src/zz9.rs")))),
        }));
        (app, path)
    }

    /// A review session with general comments has a Conversation first in
    /// the tree, counted there and not as threads; the files keep their
    /// place around it, and it goes with the last general comment. A local
    /// review shows none: nothing there comes from a host.
    #[test]
    fn the_conversation_comes_first_in_a_review_with_general_comments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        std::fs::write(&path, with_conversation(&review_json("src/zz9.rs"))).expect("write");
        app.event(Event::Comments);
        app.resize(160, 30);
        let local = screen(&mut app);
        assert!(!app.doc.document.files()[0].is_page(), "{local}");
        assert!(
            !local.contains("Conversation") && local.contains("6 threads ─"),
            "{local}"
        );

        app.doc.goto_file(3);
        let on = app.doc.document.files()[3].change().path.clone();
        let built = comments::conversed_count();
        let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
        said(&mut app, prepare, Said::Done(Ok(())));
        let shown = screen(&mut app);
        assert!(app.doc.document.files()[0].is_page());
        let file = app.doc.document.pos.file;
        assert_eq!(
            (file, &app.doc.document.files()[file].change().path),
            (4, &on),
            "on its file still"
        );
        let first = shown.lines().nth(1).unwrap_or_default();
        assert!(
            first.contains("≡ Conversation") && first.contains(" 4 "),
            "{shown}"
        );
        assert!(shown.contains("6 threads · 5 drafts"), "{shown}");
        assert_eq!(
            comments::conversed_count(),
            built,
            "nothing made until it shows"
        );

        std::fs::write(&path, review_json("src/zz9.rs")).expect("write");
        app.event(Event::Comments);
        let shown = screen(&mut app);
        assert!(!app.doc.document.files()[0].is_page(), "{shown}");
        let file = app.doc.document.pos.file;
        assert_eq!(
            (file, &app.doc.document.files()[file].change().path),
            (3, &on)
        );
    }

    /// The Conversation shows the review's title, then its description and
    /// the other general comments by date, each in a thread's box with its
    /// kind and label. Its comments are made once, however it scrolls, and
    /// they're read-only.
    #[test]
    fn the_conversation_page_shows_the_title_then_each_comment_by_date() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = conversing(&dir, (72, 40));
        let written = std::fs::read(&path).expect("read");
        let built = comments::conversed_count();
        app.open(0);
        insta::assert_snapshot!("conversation_page", screen(&mut app));
        assert_eq!(comments::conversed_count(), built + 1);
        for _ in 0..6 {
            key(&mut app, 'j');
            screen(&mut app);
        }
        key(&mut app, 'g');
        screen(&mut app);
        assert_eq!(comments::conversed_count(), built + 1, "made once");

        key(&mut app, 'm');
        assert_eq!(at(&app), (0, "d1".to_owned()));
        key(&mut app, 'r');
        assert!(status(&mut app).contains("read-only"));
        assert!(app.doc.writing.is_none());
        key(&mut app, ' ');
        assert_eq!(press(&mut app, Key::Char('E')), None, "nor in the editor");
        assert!(app.composing.is_none());
        assert_eq!(std::fs::read(&path).expect("read"), written);
    }

    /// On the Conversation, with no code and no gutter, a drag in a comment
    /// copies its Markdown as on a file's thread, across its paragraphs.
    #[test]
    fn a_drag_on_the_conversation_copies_a_comment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = conversing(&dir, (72, 40));
        app.open(0);
        let (x, y) = cell_of(&mut app, "five.");
        let (tx, ty) = cell_of(&mut app, "count");
        app.event(Event::Input(Input::Click { x, y }));
        app.event(Event::Input(Input::Drag { x: tx + 4, y: ty }));
        assert_eq!(
            app.event(Event::Input(Input::Release { x: tx + 4, y: ty })),
            Some(copied("five.\n\nThe count", "copied 16 chars of a comment"))
        );
    }

    /// The comments panel lists the general comments first, apart from the
    /// files' threads, which alone its title counts; picking one shows it
    /// on the Conversation.
    #[test]
    fn the_comments_panel_lists_the_conversation_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = conversing(&dir, (130, 30));
        key(&mut app, 'i');
        let listed = left(&mut app);
        let lines: Vec<&str> = listed.lines().collect();
        assert!(
            lines[0].starts_with("┌ Comments · 6 threads · 5 open ─"),
            "{listed}"
        );
        assert!(
            lines[1].contains("Conversation") && lines[1].trim_end().ends_with("4 │"),
            "{listed}"
        );
        let order: Vec<usize> = ["Spells out", "Thanks for", "Is this ready", "Loo"]
            .iter()
            .map(|t| lines.iter().position(|l| l.contains(t)).expect(t))
            .collect();
        assert!(order.is_sorted(), "{listed}");
        assert!(lines[order[3]].contains("review approved"), "{listed}");
        assert!(lines[order[3] + 1].contains(FILE.rsplit('/').next().expect("a name")));
        key(&mut app, 'g');
        assert_eq!(at(&app), (0, "d1".to_owned()));
        key(&mut app, 'f');
        let open = left(&mut app);
        assert!(
            !open.contains("Conversation"),
            "the filters keep threads: {open}"
        );
    }

    /// `i` turns the left panel to the comment threads, showing it when
    /// it's hidden, with the keys, and back to the files; its title says
    /// which. Nothing of the list is made while the files show, however
    /// the comments change.
    #[test]
    fn i_switches_the_left_panel_between_files_and_comments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = review_json("src/zz9.rs");
        let mut app = reviewed(&dir, &json, (100, 20));
        key(&mut app, 't');
        assert!(!app.tree_shown);
        let made = comments::listed_count();
        key(&mut app, 'i');
        assert!(app.tree_shown && app.pane == Pane::Tree);
        let shown = screen(&mut app);
        assert!(shown.starts_with("┌ Comments · 6 threads ─"), "{shown}");
        assert_eq!(comments::listed_count(), made + 1);
        assert!(
            app.take_count_pass().is_none(),
            "no counting for a tree not shown"
        );
        key(&mut app, 'i');
        assert!(app.panel.is_none() && app.pane == Pane::Tree);
        assert!(app.take_count_pass().is_some());
        let shown = screen(&mut app);
        assert!(shown.starts_with("┌ Files · 12 "), "{shown}");
        let path = dir.path().join("comments.json");
        std::fs::write(&path, json.replace("Why two?", "Why 2?")).expect("write");
        app.event(Event::Comments);
        assert!(screen(&mut app).contains("Why 2?"));
        assert_eq!(comments::listed_count(), made + 1);
    }

    /// On a narrow screen the comments open over the diff; `enter` closes
    /// them on the thread, and `i` brings them back with the keys.
    #[test]
    fn narrow_comments_open_over_the_diff_and_come_back_with_i() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = reviewed(&dir, &review_json("src/zz9.rs"), (40, 30));
        key(&mut app, 'i');
        assert!(app.tree_shown && app.pane == Pane::Tree);
        key(&mut app, 'j');
        press(&mut app, Key::Enter);
        assert!(!app.tree_shown);
        assert_eq!((app.pane, at(&app)), (Pane::Diff, (0, "d1".to_owned())));
        key(&mut app, 'i');
        assert!(app.tree_shown && app.pane == Pane::Tree);
        let shown = screen(&mut app);
        assert!(
            shown
                .lines()
                .nth(1)
                .is_some_and(|l| l.starts_with("┌ Comments")),
            "{shown}"
        );
    }

    /// A thread listed before its file's diff is in, one on the whole
    /// file: following it opens the file and waits, as `m` does, then
    /// focuses it once the file's threads are placed.
    #[test]
    fn following_a_thread_whose_file_isnt_diffed_yet_waits_for_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        let json = review_json("src/zz9.rs");
        let whole = r#"{"id":"z1","path":"src/zz9.rs","text":"On the far file","created":"2026-11-17T09:00:00Z"}"#;
        let json = format!("{},{whole}]}}", json.strip_suffix("]}").expect("a list"));
        std::fs::write(&path, json).expect("write");
        app.event(Event::Comments);
        screen(&mut app);
        key(&mut app, 'i');
        for _ in 0..4 {
            key(&mut app, 'j');
        }
        assert_eq!(selected(&app), "z1");
        assert_eq!(at(&app), (11, String::new()));
        assert!(status(&mut app).contains("waiting for this file's diff"));
        app.event(Event::Diffed(Done::Diff {
            file: 11,
            version: app.version(11),
            result: Ok(Box::new(loaded(diff("src/zz9.rs")))),
        }));
        assert_eq!(at(&app), (11, "z1".to_owned()));
    }

    /// The comments beside the diff: a header per file (its folder dimmed,
    /// its name bold, its threads), then a row per thread: its mark, its
    /// line, the author when the row above in the file has another, the
    /// first line, and its replies at the right (`↩1`). A thread whose lines
    /// weren't found says `moved`, unless its file isn't in the diff. A
    /// narrow panel leaves out the line, then the replies.
    #[test]
    fn the_comments_panel_lists_one_quiet_row_per_thread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = reviewed(&dir, &review_json("src/zz9.rs"), (130, 22));
        app.tree_width = Some(46);
        app.relayout();
        key(&mut app, 'i');
        insta::assert_snapshot!("comment_list", screen(&mut app));
        let narrow: Vec<String> = [26, 24, 22, 20]
            .into_iter()
            .map(|width| {
                app.tree_width = Some(width);
                app.relayout();
                left(&mut app)
                    .lines()
                    .take(11)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect();
        insta::assert_snapshot!("comment_list_narrow", narrow.join("\n\n"));
    }

    /// A thread's row has its mark in its look's role, its line in the
    /// line numbers', `moved` dimmed in the warning's, and its author bold;
    /// a resolved one is dimmed whole, its author still bold. File names
    /// are bold.
    #[test]
    fn comment_rows_take_their_roles() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = reviewed(&dir, &review_json("src/zz9.rs"), (130, 22));
        app.tree_width = Some(46);
        app.relayout();
        key(&mut app, 'i');
        // Distinct styles for the roles the marks and rows take.
        app.theme = Theme::parse(
            "roles",
            "warn = yellow underline\nhint = cyan underline\ndim = dim underline\n\
             line-number = blue underline\n",
            &mut Vec::new(),
        );
        app.invalidate();
        let ink = |role: Role, attributes: Attributes, label: &str| {
            let ink = Ink {
                role,
                syntax: None,
                attributes,
            };
            (app.theme.ink_style(ink), label.to_owned())
        };
        let styles = [
            ink(Role::Warn, Attributes::NONE, "warn"),
            ink(Role::Hint, Attributes::NONE, "hint"),
            ink(Role::Dim, Attributes::NONE, "dim"),
            ink(Role::LineNumber, Attributes::NONE, "line-number"),
            ink(Role::Warn, Attributes::DIM, "warn+dim"),
            ink(Role::Dim, Attributes::BOLD, "dim+bold"),
            ink(Role::Plain, Attributes::BOLD, "bold"),
        ];
        let markup = style_markup(&mut app, &styles, 46);
        insta::assert_snapshot!(
            "comment_list_roles",
            markup
                .lines()
                .skip(1)
                .take(9)
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// Moving through the comments shows each thread in the diff, focused,
    /// loading its file when it's another, while the keys stay in the
    /// panel; opening it moves nothing. A thread on a file not in the diff
    /// says so and leaves nothing focused. The wheel and a tap move the
    /// selection too.
    #[test]
    fn moving_through_the_comments_shows_each_thread_in_the_diff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
        assert_eq!(selected(&app), "f1");
        assert_eq!(at(&app), (0, String::new()), "opening moves nothing");
        let mut seen = Vec::new();
        for _ in 0..5 {
            key(&mut app, 'j');
            seen.push(at(&app));
        }
        let at_ = |file: usize, root: &str| (file, root.to_owned());
        assert_eq!(
            seen,
            [
                at_(0, "d1"),
                at_(0, "p1"),
                at_(0, "r1"),
                at_(11, "m1"),
                at_(11, "")
            ]
        );
        assert!(status(&mut app).contains("r reply · e edit · d delete · x resolve"));
        assert_eq!(app.pane, Pane::Tree);
        key(&mut app, 'k');
        assert_eq!(at(&app), at_(11, "m1"));
        key(&mut app, 'g');
        assert_eq!(at(&app), at_(0, "f1"));
        app.event(Event::Input(Input::Scroll {
            delta: 1,
            x: 5,
            y: 5,
        }));
        assert_eq!((selected(&app), at(&app)), ("d1".to_owned(), at_(0, "d1")));
        // Under the header: f1, d1, p1, then r1.
        app.event(Event::Input(Input::Click { x: 5, y: 5 }));
        assert_eq!((selected(&app), at(&app)), ("r1".to_owned(), at_(0, "r1")));
        // The selection follows `m` in the diff.
        press(&mut app, Key::Tab);
        key(&mut app, 'm');
        assert_eq!(at(&app), at_(0, "p1"));
        screen(&mut app);
        assert_eq!(selected(&app), "p1");
    }

    /// Following a folded thread from the comments shows it whole until the
    /// selection moves on. `z` on it in the diff keeps what it sets, a
    /// reply keeps it whole, and closing the panel ends the peek.
    #[test]
    fn following_unfolds_a_folded_thread_while_its_selected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
        // f1, d1, then p1, outdated and posted, and r1, resolved.
        key(&mut app, 'j');
        key(&mut app, 'j');
        assert_eq!(selected(&app), "p1");
        assert!(!app.doc.fold.collapsed("p1"));
        key(&mut app, 'j');
        assert!(app.doc.fold.collapsed("p1") && !app.doc.fold.collapsed("r1"));
        press(&mut app, Key::Tab);
        key(&mut app, 'z');
        assert!(app.doc.fold.collapsed("r1"));
        press(&mut app, Key::Tab);
        key(&mut app, 'k');
        key(&mut app, 'j');
        assert_eq!(selected(&app), "r1");
        assert!(app.doc.fold.collapsed("r1"), "as z left it");
        key(&mut app, 'k');
        press(&mut app, Key::Tab);
        key(&mut app, 'r');
        assert!(app.doc.writing.is_some());
        press(&mut app, Key::Esc);
        press(&mut app, Key::Tab);
        key(&mut app, 'j');
        assert_eq!(selected(&app), "r1");
        assert!(!app.doc.fold.collapsed("p1"), "replied in");
        key(&mut app, 'i');
        assert!(app.panel.is_none());
        assert!(app.doc.fold.collapsed("r1"));
    }

    /// `enter` or `tab` hands the keys to the diff, the thread focused for
    /// `r`, `x`, `e`, `d` and `o`; `tab` comes back to the comments, and
    /// `esc` in them hands the keys to the diff as it does from the tree.
    #[test]
    fn enter_or_tab_hands_the_keys_to_the_diff_on_the_thread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
        let path = dir.path().join("comments.json");
        key(&mut app, 'j');
        press(&mut app, Key::Enter);
        assert_eq!((app.pane, at(&app)), (Pane::Diff, (0, "d1".to_owned())));
        key(&mut app, 'x');
        assert!(
            read_comments(&path)
                .comments
                .iter()
                .any(|c| c.id == "d1" && c.resolved)
        );
        press(&mut app, Key::Tab);
        assert!(app.pane == Pane::Tree && app.panel.is_some());
        assert!(screen(&mut app).starts_with("┌ Comments"));
        key(&mut app, 'j');
        let root = selected(&app);
        press(&mut app, Key::Tab);
        assert_eq!((app.pane, at(&app).1), (Pane::Diff, root));
        press(&mut app, Key::Tab);
        press(&mut app, Key::Esc);
        assert!(app.pane == Pane::Diff && app.panel.is_some());
    }

    /// On a thread whose file isn't in the diff, the panel's keys act on
    /// the selected row: `x` resolves it, `r` and `e` write in the editor,
    /// as the diff has no box to show, and `d` twice deletes the draft,
    /// then the thread. The status bar lists them; `d` then a move keeps
    /// the draft, and the search still takes every letter.
    #[test]
    fn the_comments_panel_acts_on_a_thread_not_in_the_diff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
        let path = dir.path().join("comments.json");
        let find = |id: &str| {
            read_comments(&path)
                .comments
                .into_iter()
                .find(|c| c.id == id)
        };
        for _ in 0..5 {
            key(&mut app, 'j');
        }
        assert_eq!(
            (selected(&app), at(&app).1),
            ("g1".to_owned(), String::new())
        );
        screen(&mut app);
        assert_eq!(
            note(&app).as_deref(),
            Some("r reply · e edit · d delete · x resolve")
        );
        app.resize(200, 30);
        let shown = status(&mut app);
        assert!(
            shown.contains("r reply") && shown.contains("f filter"),
            "{shown}"
        );
        key(&mut app, 'x');
        assert!(find("g1").is_some_and(|c| c.resolved));
        assert_eq!((app.pane, selected(&app)), (Pane::Tree, "g1".to_owned()));
        app.message = None;
        screen(&mut app);
        assert!(note(&app).is_some_and(|n| n.ends_with("x reopen")));
        key(&mut app, 'x');
        assert!(find("g1").is_some_and(|c| !c.resolved));

        assert_eq!(
            press(&mut app, Key::Char('r')),
            Some(Action::Compose(String::new()))
        );
        app.composed(Some("Noted".to_owned()));
        let reply = read_comments(&path)
            .comments
            .into_iter()
            .find(|c| c.text == "Noted")
            .expect("the reply");
        assert_eq!(reply.in_reply_to.as_deref(), Some("g1"));
        assert_eq!(
            press(&mut app, Key::Char('e')),
            Some(Action::Compose("Noted".to_owned()))
        );
        app.composed(Some("Noted!".to_owned()));
        assert_eq!(find(&reply.id).map(|c| c.text), Some("Noted!".to_owned()));

        key(&mut app, 'd');
        key(&mut app, 'k');
        key(&mut app, 'd');
        assert!(find("m1").is_some(), "a move disarms d");
        key(&mut app, 'j');
        key(&mut app, 'd');
        key(&mut app, 'd');
        assert!(find(&reply.id).is_none() && find("g1").is_some());
        key(&mut app, 'o');
        assert!(screen(&mut app).contains("this thread has no link"));
        key(&mut app, 'd');
        key(&mut app, 'd');
        assert!(find("g1").is_none());
        assert!(!left(&mut app).contains("Gone"));

        let before = read_comments(&path);
        app.message = None;
        key(&mut app, '/');
        assert_eq!(selected(&app), "m1");
        assert!(!note(&app).is_some_and(|n| n.contains("r reply")), "typing");
        typed(&mut app, "rexdo");
        assert!(app.panel.as_ref().is_some_and(|p| p.editing));
        assert!(left(&mut app).contains("no thread matches /rexdo"));
        assert_eq!(read_comments(&path), before, "the search takes them");
    }

    /// On a thread the diff shows, the panel's `r` and `e` open the box in
    /// the diff, which takes the keys, and `x` acts without leaving the
    /// panel.
    #[test]
    fn the_comments_panel_writes_in_the_diff_on_a_thread_shown_there() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
        let path = dir.path().join("comments.json");
        assert_eq!(selected(&app), "f1");
        assert_eq!(at(&app).1, "", "opening follows nothing");
        key(&mut app, 'j');
        assert_eq!(selected(&app), "d1");
        key(&mut app, 'x');
        assert!(
            read_comments(&path)
                .comments
                .iter()
                .any(|c| c.id == "d1" && c.resolved)
        );
        assert_eq!(app.pane, Pane::Tree);
        assert_eq!(press(&mut app, Key::Char('r')), None);
        let writes = |app: &App| app.doc.writing.as_ref().map(|w| w.at.clone());
        assert_eq!(writes(&app), Some(Writes::Reply("d1".to_owned())));
        assert_eq!(app.pane, Pane::Diff);
        typed(&mut app, "Sure");
        press(&mut app, Key::Esc);
        assert!(
            read_comments(&path)
                .comments
                .iter()
                .any(|c| c.text == "Sure" && c.in_reply_to.as_deref() == Some("d1"))
        );
        // The thread the panel opens on isn't followed until it writes.
        key(&mut app, 'i');
        app.doc.set_focus(None);
        key(&mut app, 'i');
        assert_eq!(
            (selected(&app), at(&app).1),
            ("f1".to_owned(), String::new())
        );
        key(&mut app, 'e');
        assert!(matches!(writes(&app), Some(Writes::Edit { root, .. }) if root == "f1"));
    }

    /// A right click on a thread's row in the comments panel selects it and
    /// lists its keys, without folding for one the diff doesn't show; each
    /// entry acts on that thread.
    #[test]
    fn the_menu_of_a_panel_row_acts_on_its_thread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = listed(&dir, &review_json("src/zz9.rs"), (100, 30));
        let path = dir.path().join("comments.json");
        right_click(&mut app, "Gone");
        assert_eq!((app.pane, selected(&app)), (Pane::Tree, "g1".to_owned()));
        assert_eq!(
            menu_rows(&app),
            [
                "Reply r",
                "Edit draft e",
                "Delete draft d",
                "Resolve x",
                "Copy comment",
                "Copy thread",
            ]
        );
        assert!(choose(&mut app, "Resolve").is_none());
        assert!(
            read_comments(&path)
                .comments
                .iter()
                .any(|c| c.id == "g1" && c.resolved)
        );
        right_click(&mut app, "Gone");
        let copied = choose(&mut app, "Copy comment");
        assert!(matches!(&copied, Some(Action::Copy { text, .. }) if text == b"Gone"));
        right_click(&mut app, "Gone");
        assert_eq!(
            choose(&mut app, "Reply"),
            Some(Action::Compose(String::new()))
        );
        app.composed(None);
        right_click(&mut app, "Gone");
        choose(&mut app, "Delete draft");
        key(&mut app, 'd');
        assert!(!read_comments(&path).comments.iter().any(|c| c.id == "g1"));
    }

    /// `f` steps through All, Open, Drafts and Resolved, `F` back; the title
    /// counts what's kept of every thread and names the filter. `/` keeps
    /// the threads whose path, author or text has what's typed, with the
    /// filter; `esc` clears it, then hands the keys to the diff. A filter
    /// that keeps nothing says so.
    #[test]
    fn the_comments_panel_filters_by_state_and_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = listed(&dir, &review_json("src/zz9.rs"), (130, 30));
        app.tree_width = Some(46);
        app.relayout();
        let title = |app: &mut App| screen(app).lines().next().unwrap_or_default().to_owned();
        assert!(title(&mut app).starts_with("┌ Comments · 6 threads · 5 open ─"));
        let texts = |app: &mut App| {
            let shown = left(app);
            [
                "On the whole",
                "Why two?",
                "Fifty?",
                "Old four",
                "Moved away",
                "Gone",
            ]
            .into_iter()
            .filter(|t| shown.contains(t))
            .collect::<Vec<_>>()
        };
        key(&mut app, 'f');
        assert_eq!(
            texts(&mut app),
            ["On the whole", "Why two?", "Fifty?", "Moved away", "Gone"]
        );
        assert!(title(&mut app).starts_with("┌ Comments · 5/6 · open ─"));
        key(&mut app, 'f');
        assert_eq!(
            texts(&mut app),
            ["On the whole", "Why two?", "Moved away", "Gone"]
        );
        key(&mut app, 'f');
        assert_eq!(texts(&mut app), ["Old four"]);
        assert_eq!(selected(&app), "r1");
        assert_eq!(at(&app).1, "r1", "the diff follows the selection");
        key(&mut app, 'F');
        key(&mut app, 'F');
        typed(&mut app, "/octo");
        assert_eq!(texts(&mut app), ["Fifty?"], "the author");
        assert!(screen(&mut app).contains("└« /octo▏ ─"));
        press(&mut app, Key::Esc);
        typed(&mut app, "/zz9");
        assert_eq!(texts(&mut app), ["Moved away"], "the path");
        let file = app.doc.document.pos.file;
        press(&mut app, Key::Down);
        app.event(Event::Input(Input::Key(Key::Char('n'), Mods::CTRL)));
        assert_eq!(
            app.doc.document.pos.file, file,
            "typing leaves the tree alone"
        );
        press(&mut app, Key::Enter);
        assert!(title(&mut app).starts_with("┌ Comments · 1/6 · open · /zz9 ─"));
        key(&mut app, 'f');
        key(&mut app, 'f');
        assert!(
            left(&mut app).contains("no thread matches"),
            "the filter keeps the search"
        );
        key(&mut app, 'F');
        key(&mut app, 'F');
        press(&mut app, Key::Esc);
        assert_eq!(app.pane, Pane::Tree, "esc clears the search first");
        typed(&mut app, "/because");
        assert_eq!(texts(&mut app), ["Why two?"], "a reply's text");
        press(&mut app, Key::Enter);
        press(&mut app, Key::Esc);
        for _ in 0..4 {
            key(&mut app, 'f');
        }
        assert_eq!(texts(&mut app).len(), 5, "round to Open");
        press(&mut app, Key::Esc);
        assert_eq!(app.pane, Pane::Diff);
    }

    /// A resolved thread with a draft reply in it is a draft: first among
    /// its file's, and kept by Drafts as well as Resolved, since posting
    /// sends it.
    #[test]
    fn a_resolved_thread_with_a_draft_in_it_lists_as_a_draft() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = review_json("src/zz9.rs");
        let reply = r#"{"id":"r2","path":"src/some/deeply/nested/module/file.rs","text":"Reopen?","author":"","created":"2026-11-17T11:00:00Z","in_reply_to":"r1"}"#;
        let json = format!("{},{reply}]}}", json.strip_suffix("]}").expect("a list"));
        let mut app = listed(&dir, &json, (100, 40));
        let mut seen = vec![selected(&app)];
        for _ in 0..3 {
            key(&mut app, 'j');
            seen.push(selected(&app));
        }
        assert_eq!(seen, ["f1", "d1", "r1", "p1"]);
        let listed = |app: &mut App| left(app).contains("Old four");
        key(&mut app, 'f');
        assert!(!listed(&mut app), "not open");
        key(&mut app, 'f');
        assert!(listed(&mut app), "a draft");
        key(&mut app, 'f');
        assert!(listed(&mut app), "resolved");
    }

    /// `m` goes from thread to thread in the order the diff shows them,
    /// focusing each, then on into the next file with comments, waiting for
    /// its diff when it has none yet; `M` goes back across files too.
    #[test]
    fn m_steps_through_threads_across_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        screen(&mut app);
        let focused = |app: &App| {
            app.doc
                .focused()
                .map(|t| t.root.clone())
                .unwrap_or_default()
        };
        let mut seen = Vec::new();
        for _ in 0..4 {
            key(&mut app, 'm');
            seen.push(focused(&app));
            let shown = screen(&mut app);
            let first = app.doc.focused().map(|t| t.notes[0].text.clone());
            assert!(shown.contains(&first.unwrap_or_default()), "{shown}");
        }
        assert_eq!(seen, ["f1", "d1", "r1", "p1"]);
        key(&mut app, 'm');
        let far = 11;
        assert_eq!(app.doc.document.pos.file, far);
        assert!(screen(&mut app).contains("waiting for this file's diff"));
        app.event(Event::Diffed(Done::Diff {
            file: far,
            version: app.version(far),
            result: Ok(Box::new(loaded(diff("src/zz9.rs")))),
        }));
        assert_eq!(focused(&app), "m1");
        key(&mut app, 'm');
        assert!(screen(&mut app).contains("no more threads below"));
        key(&mut app, 'M');
        assert_eq!(
            (app.doc.document.pos.file, focused(&app)),
            (0, "p1".to_owned())
        );
        key(&mut app, 'M');
        assert_eq!(focused(&app), "r1");
    }

    /// While a thread has the focus, `x` resolves and reopens it, `r` opens
    /// a reply box in it, `e` edits your draft there, `d` twice deletes the
    /// draft, and `esc` leaves. Without the focus the same keys keep their
    /// own meaning: `r` the rendered view, `x` more lines.
    #[test]
    fn a_focused_thread_takes_its_keys_only_while_focused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        let find = |id: &str| {
            read_comments(&path)
                .comments
                .into_iter()
                .find(|c| c.id == id)
        };
        screen(&mut app);
        key(&mut app, 'r');
        assert!(screen(&mut app).contains("no renderer for .rs"));
        key(&mut app, 'x');
        assert!(app.doc.writing.is_none());
        assert!(find("d1").is_some_and(|c| !c.resolved));

        key(&mut app, 'm');
        key(&mut app, 'm');
        assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("d1"));
        app.message = None;
        let shown = screen(&mut app);
        assert!(
            shown.contains("r reply · e edit · d delete · x resolve"),
            "{shown}"
        );
        let hint = note(&app).unwrap_or_default();
        assert!(hint.ends_with("z fold · esc"), "{hint}");
        key(&mut app, 'z');
        let hint = note(&app).unwrap_or_default();
        assert!(hint.ends_with("z unfold · esc"), "{hint}");
        key(&mut app, 'z');
        key(&mut app, 'x');
        assert!(find("d1").is_some_and(|c| c.resolved));
        key(&mut app, 'x');
        assert!(find("d1").is_some_and(|c| !c.resolved));

        key(&mut app, 'r');
        typed(&mut app, "Sure");
        insta::assert_snapshot!("comment_reply", screen(&mut app));
        press(&mut app, Key::Esc);
        let reply = read_comments(&path)
            .comments
            .into_iter()
            .find(|c| c.text == "Sure")
            .expect("the reply");
        assert_eq!(reply.in_reply_to.as_deref(), Some("d1"));
        assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("d1"));

        key(&mut app, 'e');
        typed(&mut app, "!");
        press(&mut app, Key::Esc);
        assert_eq!(find(&reply.id).map(|c| c.text), Some("Sure!".to_owned()));
        key(&mut app, 'd');
        assert!(find(&reply.id).is_some(), "one d asks");
        key(&mut app, 'd');
        assert!(find(&reply.id).is_none() && find("d1").is_some());
        key(&mut app, 'o');
        assert!(screen(&mut app).contains("this thread has no link"));
        press(&mut app, Key::Esc);
        assert!(app.doc.focused().is_none());

        // Scrolled off screen, the thread lets go: `x` reveals lines again.
        assert!(app.doc.go_to_thread("d1"));
        press(&mut app, Key::PageDown);
        key(&mut app, 'x');
        assert!(app.doc.focused().is_none());
        assert!(find("d1").is_some_and(|c| !c.resolved));
    }

    /// A thread placed on lines a `⋯` separator hides reveals them, two
    /// lines around them and no more: it hangs under its own line, between
    /// a separator for the hidden lines above and one for those below, and
    /// the view stays on the row it showed. `X` and back keeps them.
    #[test]
    fn a_thread_on_hidden_lines_reveals_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("comments.json");
        std::fs::write(
            &path,
            r#"{"version":1,"comments":[{"id":"h1","path":"src/some/deeply/nested/module/file.rs","side":"new","start":{"old":20,"new":20},"end":{"old":20,"new":20},"text":"Hidden here","author":"","created":"2026-11-17T10:00:00Z","anchor":{"before":[],"lines":["line 20"],"after":[]}}]}"#,
        )
        .expect("write");
        let mut app = app(100, 12);
        // The `@@` row and the first hunk's eight, then the separator.
        app.doc.scroll(9);
        let top = |app: &mut App| screen(app).lines().nth(1).unwrap_or_default().to_owned();
        assert!(top(&mut app).contains("⋯ 39 unchanged lines"));
        let line = app.doc.line_at_top();
        app.set_comments(path);
        assert_eq!(app.doc.line_at_top(), line, "the view stays");
        let shown = screen(&mut app);
        assert!(top(&mut app).contains("⋯ 10 unchanged lines"), "{shown}");
        assert!(shown.contains("⋯ 24 unchanged lines"), "{shown}");
        let has_line = |n: usize| shown.contains(&format!("│ line {n}"));
        assert!(
            (18..=22).all(has_line) && !has_line(17) && !has_line(23),
            "{shown}"
        );
        let under = shown
            .lines()
            .skip_while(|l| !l.contains("│ line 20"))
            .nth(1)
            .unwrap_or_default();
        assert!(under.contains("▾ you · 2h ago ─"), "{shown}");
        key(&mut app, 'X');
        key(&mut app, 'X');
        assert!(screen(&mut app).contains("⋯ 10 unchanged lines"));
        key(&mut app, 'm');
        let shown = screen(&mut app);
        let under = shown
            .lines()
            .skip_while(|l| !l.contains("│ line 20"))
            .nth(1)
            .unwrap_or_default();
        assert!(under.contains("▾ you · 2h ago ━"), "{shown}");
    }

    /// `o` opens a posted thread on its host. Its comments are the host's:
    /// `e` and `d` say so. Replying to an outdated thread says the review
    /// plugin's warning once, when it has one.
    #[test]
    fn posted_threads_open_on_their_host_and_outdated_ones_warn() {
        let quiet = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&quiet);
        screen(&mut app);
        for _ in 0..4 {
            key(&mut app, 'm');
        }
        key(&mut app, 'r');
        assert!(!screen(&mut app).contains("outdated:"), "no warning to say");

        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        described(
            &mut app,
            r#"{"version":1,"command":"pr","noun":"pull request","nouns":"pull requests","outdated_reply_warning":"the host folds this thread, so a reply there is easy to miss"}"#,
        );
        screen(&mut app);
        for _ in 0..4 {
            key(&mut app, 'm');
        }
        assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("p1"));
        assert_eq!(
            press(&mut app, Key::Char('o')),
            Some(Action::Browse(
                "https://github.com/o/r/pull/1#discussion_r1".to_owned()
            ))
        );
        key(&mut app, 'e');
        assert!(screen(&mut app).contains("no draft here to edit"));
        key(&mut app, 'd');
        assert!(screen(&mut app).contains("no draft here to delete"));
        key(&mut app, 'r');
        assert!(screen(&mut app).contains("outdated: the host folds this thread"));
        press(&mut app, Key::Esc);
        key(&mut app, 'r');
        assert!(!screen(&mut app).contains("the host folds"), "once");
        press(&mut app, Key::Esc);
        assert_eq!(
            read_comments(&path).comments.len(),
            7,
            "empty replies discarded"
        );
    }

    /// The cell where `text` first shows on screen under the title row,
    /// which names the file shown.
    fn cell_of(app: &mut App, text: &str) -> (u16, u16) {
        let shown = screen(app);
        let (x, y) = shown
            .lines()
            .enumerate()
            .skip(1)
            .find_map(|(y, l)| Some((l[..l.find(text)?].chars().count(), y)))
            .unwrap_or_else(|| panic!("{text}:\n{shown}"));
        (
            u16::try_from(x).expect("column"),
            u16::try_from(y).expect("row"),
        )
    }

    fn right_click(app: &mut App, text: &str) {
        let (x, y) = cell_of(app, text);
        app.event(Event::Input(Input::RightClick { x, y }));
    }

    /// The open menu's entries, each its label and its key.
    fn menu_rows(app: &App) -> Vec<String> {
        app.menu
            .as_ref()
            .expect("a menu")
            .entries()
            .iter()
            .map(|e| format!("{} {}", e.label, e.key).trim_end().to_owned())
            .collect()
    }

    /// Moves the menu's cursor to the entry starting `label`, `j` and
    /// `down` alike, and runs it with `enter`.
    fn choose(app: &mut App, label: &str) -> Option<Action> {
        let i = menu_rows(app)
            .iter()
            .position(|r| r.starts_with(label))
            .unwrap_or_else(|| panic!("{label}: {:?}", menu_rows(app)));
        for step in 0..i {
            if step % 2 == 0 {
                key(app, 'j');
            } else {
                press(app, Key::Down);
            }
        }
        let done = press(app, Key::Enter);
        assert!(app.menu.is_none(), "running an entry closes the menu");
        done
    }

    /// A right click on a file in the tree selects it as `j`/`k` would
    /// and lists what the tree's keys do to it; each entry does what its
    /// key does there.
    #[test]
    fn the_menu_of_a_tree_file_runs_the_trees_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        right_click(&mut app, "zz3.rs");
        let file = app.doc.document.pos.file;
        assert_eq!(app.doc.document.files()[file].change().path, "src/zz3.rs");
        assert_eq!(app.pane, Pane::Tree);
        assert_eq!(app.tree.item(), Some(Item::File(file)));
        assert_eq!(
            menu_rows(&app),
            [
                "Copy path Y",
                "Copy absolute path",
                "Open in editor e",
                "Mark viewed v",
                "Comment on file C",
                "Show only files with comments c",
            ]
        );
        let copied = choose(&mut app, "Copy path");
        assert_eq!(copied, press(&mut app, Key::Char('Y')));
        assert_eq!(
            choose_again(&mut app, "zz3.rs", "Copy absolute path"),
            Some(Action::CopyFullPath("src/zz3.rs".into()))
        );
        assert!(choose_again(&mut app, "zz3.rs", "Mark viewed").is_none());
        assert!(app.doc.document.files()[file].viewed());
        right_click(&mut app, "zz3.rs");
        assert!(menu_rows(&app).contains(&"Mark unviewed v".to_owned()));
        esc(&mut app);
        assert!(app.menu.is_none());
        key(&mut app, 'v');
        assert!(!app.doc.document.files()[file].viewed(), "the same as v");
        choose_again(&mut app, "zz3.rs", "Show only files");
        assert!(app.tree.commented());
        // A tap on an entry runs it; a tap outside closes the menu.
        right_click(&mut app, "zz9.rs");
        let (x, y) = cell_of(&mut app, "Show every file");
        app.event(Event::Input(Input::Click { x, y }));
        assert!(app.menu.is_none() && !app.tree.commented());
        right_click(&mut app, "zz3.rs");
        app.event(Event::Input(Input::Click { x: 99, y: 0 }));
        assert!(app.menu.is_none() && !app.tree.commented());
    }

    fn choose_again(app: &mut App, text: &str, label: &str) -> Option<Action> {
        right_click(app, text);
        choose(app, label)
    }

    /// On a line of code: a comment on it, its copy and the editor at it,
    /// each the same as tapping the line and pressing the keys shown.
    #[test]
    fn the_menu_of_a_code_line_runs_its_keys_on_that_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        right_click(&mut app, "line 3");
        assert_eq!(app.pane, Pane::Diff);
        assert_eq!(
            menu_rows(&app),
            [
                "Comment on line c",
                "Suggest change S",
                "Copy line v y",
                "Open in editor e"
            ]
        );
        let copied = choose(&mut app, "Copy line");
        assert!(app.doc.selection.is_none());
        let tap = |app: &mut App, text: &str| {
            let (x, y) = cell_of(app, text);
            app.event(Event::Input(Input::Click { x, y }));
            app.event(Event::Input(Input::Release { x, y }));
        };
        tap(&mut app, "line 3");
        key(&mut app, 'v');
        assert_eq!(copied, press(&mut app, Key::Char('y')));
        assert!(matches!(&copied, Some(Action::Copy { text, .. }) if text == b"line 3"));

        let edit = choose_again(&mut app, "line 3", "Open in editor");
        assert_eq!(
            edit,
            Some(Action::Edit {
                path: FILE.into(),
                line: 3
            })
        );
        tap(&mut app, "line 3");
        assert_eq!(edit, press(&mut app, Key::Char('e')));

        choose_again(&mut app, "line 3", "Comment on line");
        let writes = |app: &App| app.doc.writing.as_ref().map(|w| w.at.clone());
        let on_line = Some(Writes::New {
            side: document::Side::New,
            lines: 2..3,
        });
        assert_eq!(writes(&app), on_line);
        esc(&mut app);
        tap(&mut app, "line 3");
        key(&mut app, 'c');
        assert_eq!(writes(&app), on_line);
    }

    /// Inside the selection the entries act on it; outside, on the line
    /// clicked, the selection gone.
    #[test]
    fn the_menu_inside_a_selection_acts_on_the_selection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        screen(&mut app);
        key(&mut app, 'v');
        key(&mut app, 'j');
        let selection = app.doc.selection.clone();
        right_click(&mut app, "line 2");
        assert_eq!(app.doc.selection, selection, "kept");
        assert_eq!(
            menu_rows(&app),
            [
                "Comment on selection c",
                "Suggest change S",
                "Copy selection y",
                "Open in editor e"
            ]
        );
        let copied = choose(&mut app, "Copy selection");
        assert!(matches!(&copied, Some(Action::Copy { text, .. }) if text == b"line 1\nline 2"));
        assert!(app.doc.selection.is_none(), "as y ends it");
        // From the line right-clicked, as after a tap.
        key(&mut app, 'v');
        key(&mut app, 'j');
        right_click(&mut app, "line 5");
        assert!(app.doc.selection.is_none());
        assert_eq!(menu_rows(&app)[0], "Comment on line c");
    }

    /// On a `⋯` separator, `x` for that one, though `x` alone reveals the
    /// first on screen: the same as tapping it.
    #[test]
    fn the_menu_of_a_separator_reveals_its_gap() {
        let tapped = |menu: bool| {
            let mut app = app(100, 40);
            let shown = screen(&mut app);
            let lines: Vec<&str> = shown.lines().collect();
            let separators: Vec<usize> = (0..lines.len())
                .filter(|&y| lines[y].contains('⋯'))
                .collect();
            let [.., _, y] = separators[..] else {
                panic!("two separators:\n{shown}");
            };
            let x = lines[y].chars().position(|c| c == '⋯').expect("⋯");
            let (x, y) = (
                u16::try_from(x).expect("column"),
                u16::try_from(y).expect("row"),
            );
            if menu {
                app.event(Event::Input(Input::RightClick { x, y }));
                assert_eq!(menu_rows(&app), ["Reveal hidden lines x"]);
                choose(&mut app, "Reveal");
            } else {
                app.event(Event::Input(Input::Click { x, y }));
            }
            screen(&mut app)
        };
        assert_eq!(tapped(true), tapped(false));
    }

    /// On a thread: what its keys do once it has the focus, as far as they
    /// apply. Edit and delete only with a draft in it, opening on the host
    /// only with a link.
    #[test]
    fn the_menu_of_a_thread_runs_its_focused_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        right_click(&mut app, "Why two?");
        assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("d1"));
        assert_eq!(
            menu_rows(&app),
            [
                "Reply r",
                "Edit draft e",
                "Delete draft d",
                "Resolve x",
                "Fold z",
                "Copy comment",
                "Copy thread",
            ]
        );
        choose(&mut app, "Edit draft");
        assert_eq!(
            app.doc.writing.as_ref().map(|w| w.text.text().to_owned()),
            Some("Because.".to_owned())
        );
        esc(&mut app);
        choose_again(&mut app, "Why two?", "Delete draft");
        assert!(status(&mut app).contains("d again deletes your draft reply"));
        key(&mut app, 'd');
        assert_eq!(read_comments(&path).comments.len(), 6);
        choose_again(&mut app, "Why two?", "Fold");
        assert!(app.doc.fold.collapsed("d1"));
        choose_again(&mut app, "Why two?", "Unfold");
        assert!(!app.doc.fold.collapsed("d1"));

        esc(&mut app);
        key(&mut app, 'g');
        for _ in 0..4 {
            key(&mut app, 'm');
        }
        esc(&mut app);
        assert!(app.doc.focused().is_none());
        right_click(&mut app, "Fifty?");
        assert_eq!(
            menu_rows(&app),
            [
                "Reply r",
                "Resolve x",
                "Unfold z",
                "Open on host o",
                "Copy comment",
                "Copy thread"
            ]
        );
        let opened = choose(&mut app, "Open on host");
        assert_eq!(opened, press(&mut app, Key::Char('o')));
        assert!(matches!(opened, Some(Action::Browse(_))));
        choose_again(&mut app, "Fifty?", "Resolve");
        assert!(
            read_comments(&path)
                .comments
                .iter()
                .any(|c| c.id == "p1" && c.resolved)
        );
    }

    /// A viewer of [`app`]'s file with a thread on its new line 2: ana's
    /// comment in Markdown, then your reply.
    fn marked(dir: &tempfile::TempDir, config: &Config) -> App {
        let mut app = app_of(config, "working tree", diff(FILE), (100, 30));
        let path = dir.path().join("comments.json");
        let json = format!(
            r#"{{"version":1,"comments":[{{"id":"d1","path":"{FILE}","side":"new","start":{{"old":null,"new":2}},"end":{{"old":null,"new":2}},"text":"Use **two blockers** at `03f1ba`.\n\n- [x] tests pass","author":"ana","created":"2026-11-17T10:00:00Z","anchor":{{"before":[],"lines":["line 2"],"after":[]}}}},{{"id":"d2","path":"{FILE}","text":"Because.","author":"","created":"2026-11-17T11:00:00Z","in_reply_to":"d1"}}]}}"#
        );
        std::fs::write(&path, json).expect("write");
        app.set_comments(path);
        app
    }

    const MARKED: &str = "Use **two blockers** at `03f1ba`.\n\n- [x] tests pass";

    fn copied(text: &str, note: &str) -> Action {
        Action::Copy {
            text: text.as_bytes().to_vec(),
            note: note.to_owned(),
        }
    }

    /// A drag in a comment selects its text by characters and copies the
    /// Markdown it was drawn from when let go. Past the comment it stops at
    /// the comment's end, not in the reply below. Neither the drag nor the
    /// frames it draws render the comment again.
    #[test]
    fn a_drag_in_a_comment_copies_its_markdown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = marked(&dir, &Config::default());
        let (x, y) = cell_of(&mut app, "two");
        let to = cell_of(&mut app, "blockers").0 + 7;
        let rendered = crate::tui::markdown::rendered_count();
        app.event(Event::Input(Input::Click { x, y }));
        for x in x + 1..=to {
            app.event(Event::Input(Input::Drag { x, y }));
            app.draw();
        }
        let done = app.event(Event::Input(Input::Release { x: to, y }));
        app.draw();
        assert_eq!(
            crate::tui::markdown::rendered_count(),
            rendered,
            "no comment rendered again"
        );
        assert_eq!(
            done,
            Some(copied("**two blockers**", "copied 16 chars of a comment"))
        );
        assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("d1"));

        let (rx, ry) = cell_of(&mut app, "Because.");
        app.event(Event::Input(Input::Click { x, y }));
        app.event(Event::Input(Input::Drag { x: rx, y: ry }));
        assert_eq!(
            app.event(Event::Input(Input::Release { x: rx, y: ry })),
            Some(copied(
                "**two blockers** at `03f1ba`.\n\n- [x] tests pass",
                "copied 47 chars of a comment"
            ))
        );
    }

    /// A click in a comment focuses its thread and copies nothing; a
    /// double click copies the word, a triple click the whole comment as
    /// written. A click on the thread's top line still folds it.
    #[test]
    fn clicks_in_a_comment_copy_a_word_or_the_comment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = marked(&dir, &Config::default());
        let (x, y) = cell_of(&mut app, "blockers");
        let click = |app: &mut App| {
            app.event(Event::Input(Input::Click { x, y }));
            app.event(Event::Input(Input::Release { x, y }))
        };
        assert_eq!(click(&mut app), None);
        assert_eq!(app.doc.focused().map(|t| t.root.as_str()), Some("d1"));
        assert!(!app.doc.fold.collapsed("d1"));
        let word = Some(copied("blockers", "copied 8 chars of a comment"));
        assert_eq!(click(&mut app), word);
        assert_eq!(press(&mut app, Key::Char('y')), word, "y copies it again");
        assert_eq!(click(&mut app), Some(copied(MARKED, "copied a comment")));
        key(&mut app, 'j');
        assert!(app.doc.note_selection.is_none(), "another key ends it");
        let (x, y) = cell_of(&mut app, "▾ ana");
        app.event(Event::Input(Input::Click { x, y }));
        app.event(Event::Input(Input::Release { x, y }));
        assert!(app.doc.fold.collapsed("d1"));
    }

    /// A drag that starts on code selects code, even when it goes on into a
    /// comment.
    #[test]
    fn a_drag_from_code_into_a_comment_copies_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = marked(&dir, &Config::default());
        let (x, y) = cell_of(&mut app, "line 2");
        let (cx, cy) = cell_of(&mut app, "two");
        app.event(Event::Input(Input::Click { x, y }));
        app.event(Event::Input(Input::Drag { x: x + 5, y }));
        app.event(Event::Input(Input::Drag { x: cx, y: cy }));
        assert_eq!(
            app.event(Event::Input(Input::Release { x: cx, y: cy })),
            Some(copied("line 2", "copied 6 chars"))
        );
    }

    /// On a thread the menu also copies the comment right-clicked as it was
    /// written, and the whole thread, each comment under its author and
    /// date. They have no keys unless bound, and bound, show them.
    #[test]
    fn the_menu_of_a_thread_copies_a_comment_or_the_thread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.apply_source("keybind = alt+c=copy_thread\n", &mut Vec::new());
        let mut app = marked(&dir, &config);
        right_click(&mut app, "Because.");
        assert_eq!(
            menu_rows(&app)[menu_rows(&app).len() - 2..],
            ["Copy comment", "Copy thread alt+c"]
        );
        assert_eq!(
            choose(&mut app, "Copy comment"),
            Some(copied("Because.", "copied a comment"))
        );
        assert_eq!(
            choose_again(&mut app, "two", "Copy comment"),
            Some(copied(MARKED, "copied a comment"))
        );
        let thread = format!("ana · 2026-11-17\n{MARKED}\n\nyou · 2026-11-17\nBecause.");
        let whole = Some(copied(&thread, "copied a thread of 2 comments"));
        assert_eq!(choose_again(&mut app, "Because.", "Copy thread"), whole);
        assert_eq!(
            app.event(Event::Input(Input::Key(Key::Char('c'), Mods::ALT))),
            whole
        );
    }

    /// `C`, or the tree menu's entry, writes a comment on the whole file in
    /// a box under its first row.
    #[test]
    fn c_on_a_file_comments_on_the_whole_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("comments.json");
        let mut app = app(100, 20);
        app.set_comments(path.clone());
        choose_again(&mut app, "file.rs", "Comment on file");
        assert_eq!(app.pane, Pane::Diff, "the box is in the diff");
        typed(&mut app, "Whole");
        let shown = screen(&mut app);
        let at = |text: &str| {
            shown
                .find(text)
                .unwrap_or_else(|| panic!("{text}:\n{shown}"))
        };
        assert!(at("Whole▏") < at("line 1"), "{shown}");
        esc(&mut app);
        let saved = read_comments(&path);
        let [comment] = saved.comments.as_slice() else {
            panic!("one comment: {saved:?}");
        };
        assert_eq!(
            (comment.path.as_str(), comment.text.as_str()),
            (FILE, "Whole")
        );
        assert!(comment.start.is_none() && comment.anchor.is_none());
        assert_eq!(app.doc.focused().map(|t| &t.at), Some(&comments::At::File));
        key(&mut app, 'C');
        assert_eq!(app.doc.writing.as_ref().map(|w| &w.at), Some(&Writes::File));
        esc(&mut app);
        // A file not diffed yet takes one too: it needs no lines.
        choose_again(&mut app, "zz.rs", "Comment on file");
        typed(&mut app, "Later");
        assert!(screen(&mut app).contains("Later▏"));
        esc(&mut app);
        assert!(
            read_comments(&path)
                .comments
                .iter()
                .any(|c| c.path == "src/zz.rs")
        );
    }

    /// The menu opens at the cell right-clicked, or as close as the screen
    /// allows.
    #[test]
    fn the_menu_opens_at_the_click_within_the_screen() {
        let mut app = app(80, 14);
        screen(&mut app);
        app.event(Event::Input(Input::RightClick { x: 10, y: 4 }));
        let shown = screen(&mut app);
        assert_eq!(
            shown.lines().nth(4).and_then(|l| l.chars().nth(10)),
            Some('┌'),
            "{shown}"
        );
        esc(&mut app);
        app.event(Event::Input(Input::RightClick { x: 77, y: 12 }));
        insta::assert_snapshot!("menu_at_the_corner", screen(&mut app));
        // A right click elsewhere opens it there instead.
        app.event(Event::Input(Input::RightClick { x: 10, y: 4 }));
        let shown = screen(&mut app);
        assert_eq!(
            shown.lines().nth(4).and_then(|l| l.chars().nth(10)),
            Some('┌'),
            "{shown}"
        );
        assert_eq!(shown.matches('┌').count(), 1, "{shown}");
    }

    /// A closed menu costs nothing: the screen is as before it opened, and
    /// the next frame reuses the diff's rows again.
    #[test]
    fn a_closed_menu_draws_nothing() {
        let mut app = app(80, 14);
        let before = screen(&mut app);
        app.event(Event::Input(Input::RightClick { x: 10, y: 4 }));
        assert_ne!(screen(&mut app), before);
        assert!(!app.kept, "an overlay over the rows");
        esc(&mut app);
        assert!(app.menu.is_none());
        assert_eq!(screen(&mut app), before);
        assert!(app.kept);
        // Nothing to act on, no menu: the status bar.
        app.event(Event::Input(Input::RightClick { x: 10, y: 13 }));
        assert!(app.menu.is_none());
    }

    /// `z` folds the focused thread to one row and back; `Z` folds all.
    #[test]
    fn z_folds_threads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        screen(&mut app);
        key(&mut app, 'm');
        key(&mut app, 'm');
        key(&mut app, 'z');
        let shown = screen(&mut app);
        assert!(
            shown.contains("▸ you · 2h ago · Why two?") && shown.contains("1 reply"),
            "{shown}"
        );
        assert!(!shown.contains("Because."));
        key(&mut app, 'z');
        assert!(screen(&mut app).contains("Because."));
        key(&mut app, 'Z');
        let shown = screen(&mut app);
        assert!(
            !shown.contains('▾') && shown.contains("▸ you · 2h ago · resolved · Old four"),
            "{shown}"
        );
    }

    /// A click on a thread's header row folds or unfolds it; a click in
    /// its body only focuses it.
    #[test]
    fn a_click_on_a_thread_header_folds_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        let shown = screen(&mut app);
        let header = shown
            .lines()
            .position(|l| l.contains("▾ you · 2h ago ─"))
            .expect("an open thread's header");
        let row = |shown: &str| shown.lines().nth(header).unwrap_or_default().to_owned();
        let click = |app: &mut App, y: usize| {
            app.event(Event::Input(Input::Click {
                x: 60,
                y: u16::try_from(y).expect("row"),
            }));
        };
        click(&mut app, header + 1);
        assert!(
            row(&screen(&mut app)).contains('▾'),
            "a body click doesn't fold"
        );
        assert!(app.doc.focused().is_some(), "a body click focuses");
        click(&mut app, header);
        let shown = screen(&mut app);
        assert!(row(&shown).contains('▸'), "{shown}");
        click(&mut app, header);
        let shown = screen(&mut app);
        assert!(row(&shown).contains('▾'), "{shown}");
    }

    /// `c` in the tree keeps only the files with comments, and `f` keeps
    /// to them; `esc` shows every file again.
    #[test]
    fn the_tree_keeps_the_files_with_comments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        screen(&mut app);
        press(&mut app, Key::Tab);
        key(&mut app, 'c');
        let shown = screen(&mut app);
        assert!(shown.contains("Files · 2/12"), "{shown}");
        assert!(
            !shown.contains("zz0.rs") && shown.contains("zz9.rs"),
            "{shown}"
        );
        key(&mut app, 'f');
        assert_eq!(app.doc.document.pos.file, 11);
        press(&mut app, Key::Esc);
        assert!(screen(&mut app).contains("zz0.rs"));
    }

    /// A paste is never read as keys: outside the text box it's dropped,
    /// but the search being typed takes its first line.
    #[test]
    fn a_paste_outside_the_box_is_never_keys() {
        let mut app = app(100, 20);
        let before = app.doc.document.pos;
        let paste = |text: &str| Event::Input(Input::Paste(text.to_owned()));
        assert_eq!(app.event(paste("qjjj")), None);
        assert_eq!(app.doc.document.pos, before);
        key(&mut app, '/');
        app.event(paste("fifty\nq"));
        let typed = app.search.as_ref().map(|s| s.needle.text.clone());
        assert_eq!(typed.as_deref(), Some("fifty"));
    }

    fn placed(app: &App, root: &str) -> bool {
        (app.comments.as_ref())
            .and_then(|c| c.threads(FILE.as_bytes()))
            .is_some_and(|threads| threads.iter().any(|t| t.root == root))
    }

    /// When another program changes the file, its threads are placed again
    /// and its notice shows.
    #[test]
    fn a_changed_comments_file_is_read_again_with_its_notice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        screen(&mut app);
        assert!(placed(&app, "p1"));
        review::apply(
            &path,
            vec![
                Op::Rm(Target::Id("p1".to_owned())),
                Op::Notice(Some("comments as of 10:42".to_owned())),
            ],
        )
        .expect("applies");
        app.event(Event::Comments);
        let shown = screen(&mut app);
        assert!(shown.contains("comments as of 10:42"), "{shown}");
        assert!(!placed(&app, "p1") && placed(&app, "d1"));
    }

    /// A review plugin's progress reads the comments file again, without
    /// waiting on the file watcher: it writes as it goes.
    #[test]
    fn a_review_runs_progress_reads_the_comments_file_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        screen(&mut app);
        assert!(placed(&app, "p1"));
        review::apply(&path, vec![Op::Rm(Target::Id("p1".to_owned()))]).expect("applies");
        let progress = Said::Progress("page 2".to_owned());
        app.event(Event::Review {
            id: 1,
            said: progress,
        });
        assert!(!placed(&app, "p1") && placed(&app, "d1"));
    }

    /// Only a review session's file has drafts: your comments not posted
    /// in it. In a local review nothing is posted, so nothing is a draft,
    /// and an agent's comment in a session is left for you, marked, rather
    /// than posted under your name.
    #[test]
    fn drafts_are_yours_in_a_review_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        review::apply(
            &path,
            vec![Op::Add {
                comment: Box::new(review::Comment {
                    path: FILE.to_owned(),
                    text: "From the agent".to_owned(),
                    author: "claude".to_owned(),
                    ..review::Comment::default()
                }),
                reply_to: Some(Target::Id("d1".to_owned())),
            }],
        )
        .expect("applies");
        app.event(Event::Comments);
        let shown = screen(&mut app);
        assert!(!shown.contains("draft") && !shown.contains('◆'), "{shown}");
        assert!(!shown.contains("not posted"), "{shown}");
        assert_eq!(app.comments.as_ref().map(Comments::counts), Some((6, 0)));

        let prepare = app.start_review("test".to_owned(), session(&dir, HEAD, None));
        said(&mut app, prepare, Said::Done(Ok(())));
        let shown = screen(&mut app);
        assert!(shown.contains("▾ you · 2h ago · draft"), "{shown}");
        assert!(shown.contains("claude · just now · not posted"), "{shown}");
        assert_eq!(app.comments.as_ref().map(Comments::counts), Some((6, 5)));
        assert_eq!(
            app.sending(true).comments,
            5,
            "the agent's reply isn't sent"
        );

        // `a` on its thread makes it yours: a draft, sent with the rest.
        app.job = None;
        app.follow_thread("d1");
        key(&mut app, 'a');
        assert!(status(&mut app).contains("now yours: 1 draft"));
        assert_eq!(app.sending(true).comments, 6);
        let authors: Vec<String> = (read_comments(&path).comments.into_iter())
            .map(|c| c.author)
            .collect();
        assert!(!authors.contains(&"claude".to_owned()), "{authors:?}");
    }

    /// The local review's file holds every branch's threads: the viewer
    /// shows the current branch's and those on none, a new one goes on the
    /// current branch, and the comments panel lists the others folded,
    /// where `b` shows them and `B` brings one here.
    #[test]
    fn local_threads_keep_to_their_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("comments.json");
        let json = format!(
            r#"{{"version":1,"comments":[
{{"id":"h1","path":"{FILE}","side":"new","start":{{"old":null,"new":2}},"end":{{"old":null,"new":2}},"text":"Here","author":"","created":"2026-11-17T10:00:00Z","branch":"main","anchor":{{"before":[],"lines":["line 2"],"after":[]}}}},
{{"id":"o1","path":"{FILE}","side":"new","start":{{"old":null,"new":2}},"end":{{"old":null,"new":2}},"text":"Elsewhere","author":"","created":"2026-11-17T10:00:00Z","branch":"topic","anchor":{{"before":[],"lines":["line 2"],"after":[]}}}},
{{"id":"o2","path":"{FILE}","text":"Its reply","author":"","created":"2026-11-17T10:01:00Z","branch":"topic","in_reply_to":"o1"}},
{{"id":"n1","path":"{FILE}","text":"Everywhere","author":"","created":"2026-11-17T10:00:00Z"}}
]}}"#
        );
        std::fs::write(&path, json).expect("write");
        let mut app = app(200, 30);
        app.set_branch(Some("main".to_owned()));
        app.set_comments(path.clone());
        let shown = screen(&mut app);
        assert!(
            shown.contains("Here") && shown.contains("Everywhere"),
            "{shown}"
        );
        assert!(!shown.contains("Elsewhere"), "{shown}");
        assert_eq!(app.comments.as_ref().map(Comments::counts), Some((2, 0)));

        key(&mut app, 'c');
        typed(&mut app, "New one");
        press(&mut app, Key::Esc);
        let added = read_comments(&path)
            .comments
            .into_iter()
            .find(|c| c.text == "New one")
            .expect("saved");
        assert_eq!(added.branch.as_deref(), Some("main"));

        key(&mut app, 'i');
        let listed = left(&mut app);
        assert!(
            listed.contains("1 thread on other branches · b lists them"),
            "{listed}"
        );
        assert!(!listed.contains("Elsewhere"), "{listed}");
        key(&mut app, 'b');
        let listed = left(&mut app);
        assert!(
            listed.contains("Other branches") && listed.contains("topic"),
            "{listed}"
        );
        for _ in 0..10 {
            if selected(&app) == "o1" {
                break;
            }
            press(&mut app, Key::Down);
        }
        app.message = None;
        app.dirty = true;
        assert!(
            status(&mut app).contains("B move here"),
            "{}",
            status(&mut app)
        );
        key(&mut app, 'B');
        let moved = read_comments(&path);
        assert!(
            moved
                .comments
                .iter()
                .filter(|c| c.id.starts_with('o'))
                .all(|c| c.branch.as_deref() == Some("main")),
            "the thread, its reply too"
        );
        press(&mut app, Key::Esc);
        assert!(screen(&mut app).contains("Elsewhere"));
    }

    /// On the review plain dffy found, a new comment is the working tree's:
    /// it goes to the local review's file, on the branch, until posting
    /// moves it.
    #[test]
    fn on_a_detected_review_new_comments_go_to_the_local_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (pr, local) = (
            dir.path().join("comments.json"),
            dir.path().join("local.json"),
        );
        std::fs::write(&pr, PR_FILE).expect("write");
        let mut app = app(100, 30);
        described(&mut app, DESCRIBED);
        app.set_branch(Some("feature".to_owned()));
        app.set_local_comments(Some(local.clone()));
        let Action::Plugin { id, .. } = app.detect_review("test".to_owned()) else {
            panic!("a run");
        };
        let (prepare, ..) = plugin_run(said(
            &mut app,
            id,
            Said::Detected(Some(session(&dir, HEAD, None))),
        ));
        said(&mut app, prepare, Said::Session(session(&dir, HEAD, None)));
        app.set_comments(pr.clone());
        screen(&mut app);
        key(&mut app, 'c');
        typed(&mut app, "Fix this");
        press(&mut app, Key::Esc);
        let written = read_comments(&local).comments;
        assert_eq!(written.len(), 1, "{written:?}");
        assert_eq!(written[0].branch.as_deref(), Some("feature"));
        assert!(
            !read_comments(&pr)
                .comments
                .iter()
                .any(|c| c.text == "Fix this")
        );
        assert!(screen(&mut app).contains("draft · local"));
    }

    /// "Suggest change" opens the box with the lines as a suggestion to
    /// edit, the cursor above it; saved, the lines go in `suggestion`, and
    /// `e` edits both.
    #[test]
    fn a_suggested_change_is_saved_as_the_comments_suggestion() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("comments.json");
        let mut app = app(100, 30);
        app.set_comments(path.clone());
        screen(&mut app);
        key(&mut app, 'S');
        let text = app.doc.writing.as_ref().map(|w| w.text.text().to_owned());
        assert_eq!(
            text.as_deref(),
            Some("\n\n```suggestion\nline 1\n```"),
            "{}",
            screen(&mut app)
        );
        typed(&mut app, "Say one");
        press(&mut app, Key::Down);
        press(&mut app, Key::Down);
        press(&mut app, Key::Down);
        press(&mut app, Key::End);
        typed(&mut app, " (one)");
        press(&mut app, Key::Esc);
        let saved = read_comments(&path).comments;
        assert_eq!(
            (saved[0].text.as_str(), saved[0].suggestion.as_deref()),
            ("Say one", Some("line 1 (one)"))
        );
        assert!(screen(&mut app).contains("suggested change:"));
        key(&mut app, 'e');
        let text = app.doc.writing.as_ref().map(|w| w.text.text().to_owned());
        assert_eq!(
            text.as_deref(),
            Some("Say one\n\n```suggestion\nline 1 (one)\n```")
        );
    }

    /// A comment written in the editor whose save fails opens in the box
    /// again where the diff shows its place, else comes back to keep.
    #[test]
    fn text_from_the_editor_is_never_lost() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("comments.json");
        let mut app = app(100, 30);
        app.set_comments(path.clone());
        std::fs::write(&path, r#"{"version":2,"comments":[]}"#).expect("write");
        screen(&mut app);
        key(&mut app, ' ');
        assert!(matches!(
            press(&mut app, Key::Char('E')),
            Some(Action::Compose(_))
        ));
        assert_eq!(app.composed(Some("Written there".to_owned())), None);
        let text = app.doc.writing.as_ref().map(|w| w.text.text().to_owned());
        assert_eq!(text.as_deref(), Some("Written there"), "back in the box");

        press(&mut app, Key::Esc);
        press(&mut app, Key::Esc);
        app.composing = Some(Compose::Reply("gone".to_owned()));
        assert_eq!(
            app.composed(Some("A reply".to_owned())),
            Some("A reply".to_owned()),
            "no box for a thread not shown: the caller keeps it"
        );
    }

    /// A signal to stop saves the box's text, as `ctrl+c` does; leaving a
    /// review with drafts not posted says so.
    #[test]
    fn an_interrupt_saves_the_box_and_quitting_names_unposted_drafts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        screen(&mut app);
        key(&mut app, 'C');
        typed(&mut app, "Half written");
        app.interrupted();
        assert!(
            read_comments(&path)
                .comments
                .iter()
                .any(|c| c.text == "Half written")
        );
        assert_eq!(app.unposted_note(), None, "not a review");
        described(&mut app, DESCRIBED);
        app.start_review("test".to_owned(), session(&dir, HEAD, None));
        assert_eq!(
            app.unposted_note().as_deref(),
            Some("PR #7: 6 drafts not posted")
        );
    }

    /// A review the host closed or merged says so in the status bar, and
    /// the dialog warns before posting on it.
    #[test]
    fn a_merged_review_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _) = commented(&dir);
        app.resize(160, 30);
        described(&mut app, DESCRIBED);
        let merged = Session {
            state: Some(plugin::review::State::Merged),
            ..session(&dir, HEAD, None)
        };
        app.start_review("test".to_owned(), merged);
        app.job = None;
        app.message = None;
        assert!(
            status(&mut app).contains("PR #7 · merged"),
            "{}",
            status(&mut app)
        );
        press(&mut app, Key::Char('P'));
        assert!(screen(&mut app).contains("! PR #7 is merged"));
    }

    /// A `pull` that brings threads says how many, and `m` goes to them
    /// first; the session pointer names the review session.
    #[test]
    fn an_update_names_the_new_threads_and_m_goes_to_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, path) = commented(&dir);
        described(&mut app, DESCRIBED);
        app.start_review("test".to_owned(), session(&dir, HEAD, None));
        app.job = None;
        assert_eq!(
            app.session_pointer(),
            Some((path.clone(), "7".to_owned(), HEAD.to_owned()))
        );
        let (pull, ..) = plugin_run(press(&mut app, Key::Char('p')));
        said(&mut app, pull, Said::Session(session(&dir, HEAD, None)));
        review::apply(
            &path,
            vec![Op::Upsert {
                fields: [
                    ("ref", "github:9"),
                    ("path", FILE),
                    ("text", "Brand new"),
                    ("author", "octocat"),
                ]
                .map(|(k, v)| (k.to_owned(), plugin::json::Value::String(v.to_owned())))
                .into(),
                reply_to: None,
            }],
        )
        .expect("applies");
        said(&mut app, pull, Said::Done(Ok(())));
        assert!(
            status(&mut app).contains("1 new comment · m goes to them"),
            "{}",
            status(&mut app)
        );
        key(&mut app, 'm');
        let new = read_comments(&path)
            .comments
            .into_iter()
            .find(|c| c.text == "Brand new")
            .expect("imported");
        assert_eq!(app.doc.focused().map(|t| t.root.clone()), Some(new.id));
    }
}

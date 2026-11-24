//! Entry points for the benchmarks in `benches/`; not an API.

use std::sync::Arc;

use crate::config::{Config, Indicators};
use crate::navigator::client::Helper;
use crate::term::{Input, Key, Mods};
use crate::theme::Theme;
use crate::tui::picker::Paths;
use crate::tui::worker::{Job, Pool, Settings};
use crate::tui::{App, Done, Event};
use dffy_core::bstr::ByteSlice;
use dffy_core::cache::Cache;
use dffy_core::diff::Options;
use dffy_core::diff::{Emphasis, Whitespace, emphasize};
use dffy_core::document::{self, Counts, File, Prepare};
use dffy_core::git::{FileChange, FileDiff, Repo, Snapshot};
use dffy_core::highlight::{Highlighter, HunkSyntax, Scopes, Source};

/// The core's per-file work as the viewer's workers run it, without the
/// disk cache.
fn prepare() -> Prepare<'static> {
    File::prepare(crate::grammars(), None)
}

/// The TUI showing one file diff, drawing frames without a terminal.
pub struct Frame {
    app: App,
}

impl Frame {
    /// A `width` × `height` viewer on `diff` with the default config and
    /// theme, its emphasis (and with `highlight`, syntax spans) already
    /// computed as if the workers had finished.
    #[must_use]
    pub fn new(diff: FileDiff, width: usize, height: usize, highlight: bool) -> Self {
        Self::with_config(diff, width, height, highlight, &Config::default())
    }

    /// [`Frame::new`] with renderer plugins configured but not asked for:
    /// the cost of the setting alone.
    ///
    /// # Panics
    ///
    /// If the setting it uses stops parsing.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "benchmark setup: the setting is a constant"
    )]
    pub fn with_idle_renderer(diff: FileDiff, width: usize, height: usize) -> Self {
        let mut config = Config::default();
        config
            .set("render", "c:bench h:bench md:bench")
            .expect("a valid render setting");
        Self::with_config(diff, width, height, false, &config)
    }

    fn with_config(
        diff: FileDiff,
        width: usize,
        height: usize,
        highlight: bool,
        config: &Config,
    ) -> Self {
        let hunks = emphases(&diff, highlight);
        let mut app = App::new(
            config,
            Theme::default_theme(),
            "bench".to_owned(),
            Some(diff.change.clone()),
            (width, height),
            Indicators::Bars,
        );
        let loaded = prepare().file(diff, Whitespace::Exact, highlight);
        let diff = Arc::clone(loaded.diff());
        let version = app.version(0);
        app.event(Event::Diffed(Done::Diff {
            file: 0,
            version,
            result: Ok(Box::new(loaded)),
            placed: None,
        }));
        for (hunk, (emphasis, syntax)) in hunks.into_iter().enumerate() {
            app.event(Event::Diffed(Done::Emphasis {
                file: 0,
                version,
                diff: Arc::clone(&diff),
                hunk,
                emphasis,
                syntax,
            }));
        }
        Self { app }
    }

    /// The work the viewer's workers do to show `diff`, all on this thread:
    /// the file's view, then every hunk's emphasis (with `highlight`, from
    /// a parse of both sides) and, with `structural`, the syntax diff.
    /// Returns the number of hunks, to keep the work observable.
    #[must_use]
    pub fn prepare(diff: FileDiff, highlight: bool, structural: bool) -> usize {
        let hunks = emphases(&diff, highlight);
        std::hint::black_box(structural.then(|| prepare().structural(&diff)));
        std::hint::black_box(prepare().file(diff, Whitespace::Exact, highlight));
        hunks.len()
    }

    /// A `width` × `height` viewer of every change in `changes` with the
    /// file tree beside it, counts known for all, showing `diff` (one of
    /// them) diffed.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "benchmark setup: a diff that isn't one of the changes is a broken benchmark"
    )]
    pub fn with_tree(
        changes: Vec<FileChange>,
        diff: FileDiff,
        width: usize,
        height: usize,
    ) -> Self {
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "bench".to_owned(),
            None,
            (width, height),
            Indicators::Bars,
        );
        let file = changes
            .iter()
            .position(|c| c.path == diff.change.path)
            .expect("the diff is one of the changes");
        let n = changes.len();
        for change in changes {
            app.event(Event::Enumerated(change));
        }
        app.event(Event::EnumerationDone(None));
        let counts = (0..n)
            .map(|i| {
                (
                    i,
                    app.version(i),
                    Counts {
                        added: i % 997,
                        removed: i % 13,
                    },
                )
            })
            .collect();
        app.event(Event::Diffed(Done::Counts { counts }));
        let version = app.version(file);
        app.event(Event::Diffed(Done::Diff {
            file,
            version,
            result: Ok(Box::new(prepare().file(diff, Whitespace::Exact, false))),
            placed: None,
        }));
        for _ in 0..file {
            app.event(Event::Input(Input::Key(Key::Char('f'), Mods::NONE)));
        }
        Self { app }
    }

    /// Runs the file tree's count pass over `changes` on the viewer's worker
    /// pool and returns each file's added and removed lines, by index.
    ///
    /// # Panics
    ///
    /// When the pool can't start or stops early.
    #[must_use]
    pub fn count_pass(repo: &Repo, changes: Vec<FileChange>) -> Vec<(usize, usize)> {
        Self::count_pass_cached(repo, changes, None)
    }

    /// [`Frame::count_pass`] with the counts found in and kept in `cache`,
    /// written by the time it returns.
    ///
    /// # Panics
    ///
    /// When the pool can't start or stops early.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "benchmark setup: without the pool there is nothing to measure"
    )]
    pub fn count_pass_cached(
        repo: &Repo,
        changes: Vec<FileChange>,
        cache: Option<Cache>,
    ) -> Vec<(usize, usize)> {
        let (tx, rx) = std::sync::mpsc::channel();
        let settings = Settings {
            options: Options::default(),
            highlight: false,
            grammars: crate::grammars(),
            cache,
        };
        let pool = Pool::new(Some(repo), &settings, &tx).expect("the pool starts");
        let mut counts = vec![(0, 0); changes.len()];
        let mut left = changes.len();
        pool.count(
            changes
                .into_iter()
                .enumerate()
                .map(|(i, change)| (i, 0, change))
                .collect(),
        );
        while left > 0 {
            if let Ok(Event::Diffed(Done::Counts { counts: chunk })) = rx.recv() {
                left -= chunk.len();
                for (file, _, c) in chunk {
                    counts[file] = (c.added, c.removed);
                }
            }
        }
        pool.flush();
        counts
    }

    /// A navigator helper, started as the viewer starts it (`program` the
    /// dffy binary built for the benchmarks), building the index of the new
    /// side of the diff between `snapshots` into `cache`.
    ///
    /// # Panics
    ///
    /// When it can't start or open the navigator.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "benchmark setup: without the helper there is nothing to measure"
    )]
    pub fn navigator(
        program: std::path::PathBuf,
        repo: &Repo,
        snapshots: [Snapshot; 2],
        cache: &Cache,
    ) -> Building {
        crate::navigator::client::use_program(program);
        let root = repo
            .workdir()
            .or_else(|| repo.git_dir())
            .expect("a repository");
        let helper = Helper::start(root, cache, 1, None).expect("the helper starts");
        helper
            .open(snapshots, document::Side::New, Whitespace::Exact)
            .expect("the navigator opens");
        Building(helper)
    }

    /// [`Frame::count_pass_cached`], with `building`'s index built in its
    /// helper meanwhile, at a lower priority, as the viewer has it.
    #[must_use]
    pub fn count_pass_beside(
        repo: &Repo,
        changes: Vec<FileChange>,
        _building: Option<&Building>,
        cache: Option<Cache>,
    ) -> Vec<(usize, usize)> {
        Self::count_pass_cached(repo, changes, cache)
    }

    pub fn press(&mut self, key: char) {
        self.app
            .event(Event::Input(Input::Key(Key::Char(key), Mods::NONE)));
    }

    /// Counts the matches of `pattern` on the lines `diff`'s hunks show,
    /// found the way the viewer's workers find them for the diff search.
    #[must_use]
    pub fn search(diff: &FileDiff, pattern: &str) -> usize {
        let needle = document::Needle::new(pattern);
        document::find(diff, None, None, &[], &needle, || false).map_or(0, |m| m.list.len())
    }

    /// Does the diff search's scan of the file shown on this thread, as a
    /// worker would, and hands the viewer its matches.
    pub fn find(&mut self) {
        for job in self.app.jobs() {
            if let Job::Find {
                file,
                version,
                diff,
                hidden,
                reading,
                blocks,
                blocks_version,
                needle,
                id,
                ..
            } = job
            {
                let matches = crate::tui::worker::find(
                    diff.as_deref(),
                    (&hidden, reading.as_ref()),
                    &blocks,
                    &needle,
                    || false,
                );
                self.app.event(Event::Diffed(Done::Found {
                    file,
                    version,
                    id,
                    diff,
                    hidden,
                    reading,
                    blocks_version,
                    matches,
                }));
            }
        }
    }

    pub fn enter(&mut self) {
        self.app
            .event(Event::Input(Input::Key(Key::Enter, Mods::NONE)));
    }

    pub fn backspace(&mut self) {
        self.app
            .event(Event::Input(Input::Key(Key::Backspace, Mods::NONE)));
    }

    /// Draws a frame and returns how many bytes it would send.
    pub fn draw(&mut self) -> usize {
        self.app.draw().map_or(0, <[u8]>::len)
    }

    /// Presses `j` (`down`) or `k`.
    pub fn scroll(&mut self, down: bool) {
        let key = if down { 'j' } else { 'k' };
        self.app
            .event(Event::Input(Input::Key(Key::Char(key), Mods::NONE)));
    }

    /// Forgets what the terminal shows, so the next frame is drawn whole.
    pub fn invalidate(&mut self) {
        self.app.invalidate();
    }
}

/// The file picker (`o`) over a diff's files and, under them, the
/// repository's other files at the diff's new side.
pub struct Picking {
    app: App,
}

impl Picking {
    /// A `width` × `height` viewer of every change in `changes`, the diff
    /// of `repo` between `snapshots`, with the picker open and the other
    /// files listed as a worker lists them.
    ///
    /// # Panics
    ///
    /// When the new side can't be listed.
    #[must_use]
    pub fn new(
        changes: Vec<FileChange>,
        repo: &Repo,
        snapshots: [Snapshot; 2],
        (width, height): (usize, usize),
    ) -> Self {
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "bench".to_owned(),
            None,
            (width, height),
            Indicators::Bars,
        );
        for change in changes {
            app.event(Event::Enumerated(change));
        }
        app.event(Event::EnumerationDone(None));
        app.set_snapshots(Some(snapshots));
        let mut picking = Self { app };
        picking.open(repo);
        picking
    }

    /// Opens the picker, its other files listed as a worker lists them.
    ///
    /// # Panics
    ///
    /// When the new side can't be listed.
    #[expect(
        clippy::expect_used,
        reason = "benchmark setup: without the list there is nothing to measure"
    )]
    pub fn open(&mut self, repo: &Repo) {
        self.press('o');
        for job in self.app.jobs() {
            if let Job::ListPaths { snapshot, id, .. } = job {
                let paths = Paths::list(repo, snapshot, || false).expect("lists");
                self.app.event(Event::Diffed(Done::Listed {
                    snapshot,
                    id,
                    result: Ok(Arc::new(paths)),
                }));
            }
        }
    }

    /// Closes the picker (`esc`).
    pub fn close(&mut self) {
        self.app
            .event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
    }

    pub fn press(&mut self, key: char) {
        self.app
            .event(Event::Input(Input::Key(Key::Char(key), Mods::NONE)));
    }

    pub fn backspace(&mut self) {
        self.app
            .event(Event::Input(Input::Key(Key::Backspace, Mods::NONE)));
    }

    /// Draws a frame and returns how many bytes it would send.
    pub fn draw(&mut self) -> usize {
        self.app.draw().map_or(0, <[u8]>::len)
    }

    /// The worker's score of the other files the picker waits for, to run
    /// with [`Scoring::run`]; `None` when it waits for none.
    #[must_use]
    pub fn scoring(&self) -> Option<Scoring> {
        self.app.jobs().into_iter().find_map(|job| match job {
            Job::Score {
                paths,
                candidates,
                query,
                ..
            } => Some(Scoring {
                paths,
                candidates,
                query,
            }),
            _ => None,
        })
    }
}

/// The tree's all-files mode over a version's files and a diff's.
pub struct AllFiles {
    paths: Arc<Paths>,
    files: document::Document<crate::tui::piped::Body>,
}

impl AllFiles {
    /// Every file at `snapshot` of `repo`, listed as a worker lists them,
    /// with `changes` as the diff's files.
    ///
    /// # Panics
    ///
    /// When the version can't be listed.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "benchmark setup: without the list there is nothing to measure"
    )]
    pub fn new(changes: Vec<FileChange>, repo: &Repo, snapshot: Snapshot) -> Self {
        let paths = Paths::list(repo, snapshot, || false).expect("lists");
        let mut files = document::Document::default();
        for change in changes {
            files.push(change, document::Source::Repo);
        }
        files.sort_by_path();
        Self {
            paths: Arc::new(paths),
            files,
        }
    }

    /// Switching the tree to all files: its top level made and flattened.
    /// Returns how many rows it shows.
    #[must_use]
    pub fn switch(&self) -> usize {
        crate::tui::bench_all_files(&self.paths, self.files.files(), None)
    }

    /// [`AllFiles::switch`], then the folder at `dir` opened.
    #[must_use]
    pub fn open(&self, dir: &str) -> usize {
        crate::tui::bench_all_files(&self.paths, self.files.files(), Some(dir.as_bytes()))
    }
}

/// A navigator helper building an index, closed when dropped.
pub struct Building(Helper);

impl Drop for Building {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Go to definition in a viewer of a range: its jobs run on this thread,
/// as the workers would run them, so their work is timed with the frame,
/// against the navigator helper `program` (the dffy binary built for the
/// benchmarks).
pub struct Peeking {
    app: App,
    repo: Repo,
    settings: Settings,
    /// The line asked from: the first of the file's first hunk, new side.
    line: usize,
}

impl Peeking {
    /// A `width` × `height` viewer of every change in `changes`, the diff
    /// of `repo` between `snapshots`, showing `path` (one of them), its
    /// definitions kept in `cache`, the navigator helper run as `program`.
    /// With `built`, the new version's navigator is made and its index
    /// built first.
    ///
    /// # Panics
    ///
    /// When `path` isn't one of the changes or can't be diffed.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "benchmark setup: a file that doesn't diff is a broken benchmark"
    )]
    pub fn new(
        changes: Vec<FileChange>,
        (repo, program): (&Repo, std::path::PathBuf),
        snapshots: [Snapshot; 2],
        path: &str,
        (cache, built): (Cache, bool),
        (width, height): (usize, usize),
    ) -> Self {
        crate::navigator::client::use_program(program);
        let mut app = App::new(
            &Config::default(),
            Theme::default_theme(),
            "bench".to_owned(),
            None,
            (width, height),
            Indicators::Bars,
        );
        let file =
            (changes.iter().position(|c| c.path == path)).expect("the file is one of the changes");
        let change = changes[file].clone();
        app.focus(path.into());
        for change in changes {
            app.event(Event::Enumerated(change));
        }
        app.event(Event::EnumerationDone(None));
        app.set_snapshots(Some(snapshots));
        let settings = Settings {
            options: Options::default(),
            highlight: true,
            grammars: crate::grammars(),
            cache: Some(cache),
        };
        let diff = repo.file_diff(change, &Options::default()).expect("diffs");
        let line = diff
            .hunks
            .first()
            .map_or(0, |h| h.new_start.saturating_sub(1));
        let version = app.version(file);
        app.event(Event::Diffed(Done::Diff {
            file,
            version,
            result: Ok(Box::new(prepare().file(diff, Whitespace::Exact, true))),
            placed: None,
        }));
        let mut peeking = Self {
            app,
            repo: repo.clone(),
            settings,
            line,
        };
        if built {
            peeking.open("kmalloc_array", true);
            peeking.press_esc();
        }
        peeking.app.draw();
        peeking
    }

    /// Peeks at `name` from the line, running the jobs it waits on until
    /// the peek shows its code. With `build`, a navigator made on the way
    /// gets its whole index first.
    #[expect(
        clippy::expect_used,
        reason = "benchmark setup: a peek that can't open is a broken benchmark"
    )]
    fn open(&mut self, name: &str, build: bool) {
        assert!(
            self.app.peek_name(name, document::Side::New, self.line),
            "the line can ask"
        );
        while !self.app.peek_settled() {
            let jobs: Vec<Job> = (self.app.jobs().into_iter())
                .filter(|j| matches!(j, Job::Navigator(_) | Job::Lookup(_) | Job::PeekCode(_)))
                .collect();
            assert!(!jobs.is_empty(), "the peek waits on no job");
            for job in jobs {
                let done = crate::tui::worker::run(job, Some(&self.repo), &self.settings);
                if build
                    && let Done::Navigator {
                        result: Ok((_, opened)),
                        ..
                    } = &done
                {
                    let helper = self.app.peek_helper().expect("started");
                    while !(helper.progress(opened.navigator).expect("answers"))
                        .progress
                        .complete()
                    {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
                self.app.event(Event::Diffed(done));
            }
        }
    }

    fn press_esc(&mut self) {
        self.app
            .event(Event::Input(Input::Key(Key::Esc, Mods::NONE)));
    }

    /// Peeks at `name` from the line, to the frame that shows it; closes
    /// it again after. Returns the frame's bytes.
    pub fn peek(&mut self, name: &str) -> usize {
        self.open(name, false);
        let bytes = self.app.draw().map_or(0, <[u8]>::len);
        self.press_esc();
        bytes
    }

    /// Opens the peek at `name` and keeps it open.
    pub fn keep_open(&mut self, name: &str) {
        self.open(name, false);
        self.app.draw();
    }

    /// Draws a whole frame and returns how many bytes it would send.
    pub fn full_frame(&mut self) -> usize {
        self.app.invalidate();
        self.app.draw().map_or(0, <[u8]>::len)
    }
}

/// A worker's score of the picker's other files.
pub struct Scoring {
    paths: Arc<Paths>,
    candidates: Arc<[usize]>,
    query: String,
}

impl Scoring {
    /// Scores them as the worker does; returns how many match.
    #[must_use]
    pub fn run(&self) -> usize {
        (self.paths.score(&self.query, &self.candidates, || false)).map_or(0, |m| m.len())
    }

    /// How many files it scores.
    #[must_use]
    pub fn candidates(&self) -> usize {
        self.candidates.len()
    }

    /// The score a query narrowed by one more character, `next`, does
    /// over the first `count` of these matches, as the picker does on the
    /// keystroke itself when they're few enough.
    #[must_use]
    pub fn narrowed(&self, next: char, count: usize) -> Self {
        let matches =
            (self.paths.score(&self.query, &self.candidates, || false)).unwrap_or_default();
        Self {
            paths: Arc::clone(&self.paths),
            candidates: matches.iter().take(count).map(|&(_, i)| i).collect(),
            query: format!("{}{next}", self.query),
        }
    }
}

/// Both sides of a file parsed for highlighting, as the first emphasis job
/// leaves them, for timing the scope job's walk of their trees alone.
pub struct Trees {
    diff: FileDiff,
    sides: [Option<Highlighter>; 2],
}

impl Trees {
    /// Parses both sides of `diff` and compiles their scope query.
    #[must_use]
    pub fn new(diff: FileDiff) -> Self {
        let source = |path, text| Source {
            path,
            text,
            id: None,
        };
        let prepare = prepare();
        let sides = [
            prepare.highlighter(source(diff.change.old_path().as_bytes(), &diff.old)),
            prepare.highlighter(source(diff.change.path.as_bytes(), &diff.new)),
        ];
        let trees = Self { diff, sides };
        std::hint::black_box(trees.scopes());
        trees
    }

    /// What the scope job does for the file (see [`Prepare::scopes`]):
    /// each parsed side's definitions, found in its tree.
    #[must_use]
    pub fn scopes(&self) -> [Option<Scopes>; 2] {
        let find = |side: &Option<Highlighter>, text| match side {
            Some(Highlighter::Tree(parsed, _)) => parsed.scopes(text),
            _ => None,
        };
        [
            find(&self.sides[0], &self.diff.old),
            find(&self.sides[1], &self.diff.new),
        ]
    }
}

/// Every hunk's emphasis and, with `highlight`, its syntax spans, as the
/// workers compute them.
fn emphases(diff: &FileDiff, highlight: bool) -> Vec<(Vec<Emphasis>, Option<HunkSyntax>)> {
    let syntax = highlight.then(|| prepare().syntax(diff, false));
    diff.hunks
        .iter()
        .map(|h| match &syntax {
            Some(s) => {
                let spans = s.hunk(&diff.old, &diff.new, h);
                (spans.emphasis(&diff.old, &diff.new, h), Some(spans))
            }
            None => (emphasize(&diff.old, &diff.new, h), None),
        })
        .collect()
}

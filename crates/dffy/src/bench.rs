//! Entry points for the benchmarks in `benches/`; not an API.

use std::sync::Arc;

use dffy_core::bstr::ByteSlice;
use dffy_core::cache::Cache;
use dffy_core::diff::Options;
use dffy_core::diff::{Emphasis, Whitespace, emphasize};
use dffy_core::document::{self, Counts, File, Prepare};
use dffy_core::git::{FileChange, FileDiff, Repo};
use dffy_core::highlight::{Highlighter, HunkSyntax, Scopes, Source};

use crate::config::{Config, Indicators};
use crate::term::{Input, Key, Mods};
use crate::theme::Theme;
use crate::tui::worker::{Job, Pool, Settings};
use crate::tui::{App, Done, Event};

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

    pub fn press(&mut self, key: char) {
        self.app
            .event(Event::Input(Input::Key(Key::Char(key), Mods::NONE)));
    }

    /// Counts the matches of `pattern` on the lines `diff`'s hunks show,
    /// found the way the viewer's workers find them for the diff search.
    #[must_use]
    pub fn search(diff: &FileDiff, pattern: &str) -> usize {
        let needle = document::Needle::new(pattern);
        document::find(diff, None, &[], &needle, || false).map_or(0, |m| m.list.len())
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
                blocks,
                blocks_version,
                needle,
                id,
                ..
            } = job
            {
                let matches =
                    crate::tui::worker::find(diff.as_deref(), &hidden, &blocks, &needle, || false);
                self.app.event(Event::Diffed(Done::Found {
                    file,
                    version,
                    id,
                    diff,
                    hidden,
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

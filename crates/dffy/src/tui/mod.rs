//! The full-screen viewer. Everything it reacts to arrives as an [`Event`]
//! on one channel: input, resizes and signals, changed files and refreshed
//! file lists from the change-set thread, diffs from the worker pool, and
//! the watch's state.
//!
//! In watch mode, changed paths go straight from the watch to the
//! change-set thread, which re-evaluates them and sends the new list; the
//! loop here only applies it.

mod app;
mod chrome;
mod commands;
mod comments;
mod editor;
mod fuzzy;
mod help;
mod menu;
mod picker;
pub(crate) mod piped;
mod textbox;
mod tree;
pub(crate) mod view;
pub(crate) mod worker;

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dffy_core::bstr::{BString, ByteSlice};
use dffy_core::diff::Options;
use dffy_core::git::{ChangeSet, FileChange, Repo, Revisions, Update};
use dffy_core::watch::{Change, Notice, Plain, Root, Watcher};

pub(crate) use app::{Action, App};
pub(crate) use worker::Done;

use crate::daemon::client::{self, Subscription};
use crate::pager::stdin::Stdin;
use crate::plugin::review::{self, Listed, Said};
use crate::print::{self, Plan};
use crate::render::{Canvas, escape};
use crate::state;
use crate::term::{self, Input, Reader, Signal, Tty};
use crate::theme::{BUILTIN, Role, Theme};
use crate::{Ctx, Error};
use worker::Request;

pub(crate) enum Event {
    Input(Input),
    Resize,
    /// SIGINT, SIGTERM or SIGHUP, or the terminal went away.
    Interrupt,
    Enumerated(FileChange),
    /// Carries the error message if the enumeration failed.
    EnumerationDone(Option<String>),
    Refreshed(Update),
    RefreshFailed(String),
    /// The watch started, degraded or moved in-process; changes themselves
    /// bypass the loop.
    Watch(client::Event),
    Diffed(Done),
    /// The next commit or file of a piped diff; [`Event::EnumerationDone`]
    /// follows the last.
    Piped(piped::Item),
    /// The list is empty, and this is what the view leaves out of the
    /// uncommitted changes, if it shows only part of them.
    Others(Option<print::Others>),
    /// A comments file may have changed on disk.
    Comments,
    /// A review plugin's run `id` said this.
    Review {
        id: u64,
        said: Said,
    },
    /// Nothing happened before the app's deadline.
    Tick,
}

/// Events handled in one go before drawing, at most; a burst (thousands
/// of files from the enumeration) still gets frames out.
const BATCH_TIME: Duration = Duration::from_millis(8);

/// The theme the TUI draws with: attributes only when color is off, since
/// the grid has no "plain" mode the way print output does.
pub(crate) fn theme(ctx: &Ctx) -> Theme {
    if ctx.color_out {
        ctx.theme.clone()
    } else {
        Theme::parse(BUILTIN[1].0, BUILTIN[1].1, &mut Vec::new())
    }
}

/// Applies `ctx` to a new app. The first-run tip is recorded as shown right
/// away so a second viewer started meanwhile doesn't show it too.
fn prepare(app: &mut App, ctx: &Ctx) {
    app.set_color(ctx.color_out);
    app.set_naming(ctx.described().map(|d| d.naming.clone()));
    app.set_view_remembered(ctx.sources.get("view") == crate::config::Source::LastUsed);
    app.start_tree(ctx.sources.get("tree") == crate::config::Source::LastUsed);
    app.set_context_lines(ctx.context_lines);
    if ctx.config.tips && !ctx.tip_shown {
        app.show_tip();
        state::write(&[(state::TIP_SHOWN, "true".to_owned())]);
    }
}

/// How the review plugin's command (`dffy pr`) starts the viewer.
pub(crate) enum ReviewStart {
    /// On the review whose `prepare` printed `session` and goes on as
    /// `run`, saying the rest on `rest`.
    Open {
        plugin: String,
        session: review::Session,
        run: review::Run,
        rest: mpsc::Receiver<Said>,
    },
    /// With the picker of `reviews` open, for a branch without a review.
    Pick {
        plugin: String,
        reviews: Vec<Listed>,
    },
    /// Plain `dffy` on a branch: `plugin` looks for its review, whose
    /// comments and status join what the viewer shows, if there's one.
    Detect { plugin: String },
}

/// Runs the viewer on `tty` for `plan` until the user quits. An empty diff
/// prints [`print::empty_message`] instead, without taking over the screen,
/// unless watching or opening a review: then the viewer waits for changes.
/// Looking for the branch's review doesn't hold an empty diff open.
#[expect(
    clippy::too_many_lines,
    reason = "the viewer's startup in order: workers, change set, first file, session"
)]
pub(crate) fn run(
    tty: &Tty,
    plan: Plan,
    revisions: Revisions,
    focus: Option<BString>,
    description: String,
    start: Option<ReviewStart>,
    ctx: &Ctx,
) -> Result<ExitCode, Error> {
    let (tx, rx) = mpsc::channel();
    let settings = worker::Settings {
        options: Options {
            context: ctx.context_lines,
            whitespace: ctx.config.whitespace,
        },
        highlight: ctx.highlight(),
        grammars: crate::grammars(),
        cache: ctx.cache.clone(),
    };
    let pool = worker::Pool::new(Some(&plan.repo), &settings, &tx)?;
    let files = Some(plan.repo.clone());
    let root = plan.repo.watch_root().ok();
    let compared = plan.repo.compared_roots();
    let commit = plan.commit.as_ref().map(|info| {
        Arc::new(dffy_core::patch::Commit::new(
            info.id.to_string(),
            print::commit_lines(info),
        ))
    });
    let (repo, selected, paths) = (plan.repo.clone(), revisions.clone(), plan.paths.clone());
    let mut set = ChangeSet::new(plan.repo, plan.spec, revisions, &plan.paths, plan.reverse);
    if ctx.config.renames {
        set = set.with_renames(ctx.cache.clone());
    }
    let leftovers = worker::Selected {
        repo: repo.clone(),
        revisions: selected.clone(),
        paths: paths.clone(),
    };
    let requests = worker::changes(set, leftovers, tx.clone())?;
    let watch = ctx.config.watch;
    // The screen is taken only once there's a file to show, so an empty
    // diff never flashes it.
    let first = loop {
        match rx.recv() {
            Ok(Event::Enumerated(change)) => break Some(change),
            Ok(Event::EnumerationDone(Some(error))) => return Err(Error::fatal(error)),
            Ok(Event::EnumerationDone(None)) | Err(_) => {
                // A commit without changes still has its header to show.
                let opened = start
                    .as_ref()
                    .is_some_and(|p| !matches!(p, ReviewStart::Detect { .. }));
                if watch || commit.is_some() || opened {
                    break None;
                }
                let mut c = Canvas::default();
                let message =
                    print::empty_message(&repo, &selected, &paths, ctx.config.default_diff);
                c.line(Role::Dim, &message);
                let mut out = Vec::new();
                c.write_to(&mut out, ctx.out_theme())?;
                io::stdout().lock().write_all(&out)?;
                return Ok(ExitCode::SUCCESS);
            }
            Ok(_) => {}
        }
    };
    let empty = first.is_none();
    let mut app = App::new(
        &ctx.config,
        theme(ctx),
        description,
        None,
        tty.size(),
        ctx.indicators(),
    );
    prepare(&mut app, ctx);
    app.set_selected(selected, ctx.config.default_diff);
    if let Some(commit) = commit {
        app.event(Event::Piped(piped::Item::Commit(commit)));
    }
    if let Some(first) = first {
        app.event(Event::Enumerated(first));
    }
    if empty {
        app.event(Event::EnumerationDone(None));
    }
    if let Some(focus) = focus {
        app.focus(focus);
    }
    // Absolute: review plugins get it, and run from the working tree's root.
    let absolute = |path: PathBuf| std::path::absolute(&path).unwrap_or(path);
    let default = repo.git_dir().map(crate::review::default_path);
    let comments = ctx
        .comments
        .clone()
        .or_else(|| default.clone())
        .map(absolute);
    // `dffy pr` reads the review's file; plain `dffy`'s is the local review's.
    let local = match &start {
        Some(ReviewStart::Open { .. }) => default.map(absolute),
        _ => comments.clone(),
    };
    app.set_local_comments(local);
    let watching = comments.and_then(|path| {
        let files = app.comment_files(path.clone());
        app.set_comments(path);
        watch_comments(files, tx.clone())
    });
    let mut review_run = None;
    let mut detect = None;
    match start {
        Some(ReviewStart::Detect { plugin }) => detect = Some(app.detect_review(plugin)),
        Some(ReviewStart::Open {
            plugin,
            session,
            run,
            rest,
        }) => {
            let id = app.start_review(plugin, session);
            let events = tx.clone();
            std::thread::Builder::new()
                .name("dffy-review-rest".into())
                .spawn(move || {
                    for said in rest {
                        if events.send(Event::Review { id, said }).is_err() {
                            return;
                        }
                    }
                })?;
            review_run = Some(run);
        }
        Some(ReviewStart::Pick { plugin, reviews }) => app.pick_review(plugin, reviews),
        None => {}
    }

    state::flush_on_panic();
    crate::plugin::stop_on_panic();
    term::install_panic_hook();
    let mut session = Session {
        watching,
        review_run,
        tty,
        reader: None,
        pool: &pool,
        suspended: Arc::new(AtomicBool::new(false)),
        files,
        root,
        compared,
        local: Vec::new(),
        requests,
        events: tx.clone(),
        watch: None,
        tmux_clipboard: std::cell::OnceCell::new(),
        gate: None,
    };
    if watch {
        app.set_watching(true);
        session.set_watch(&mut app, true);
    }
    if let Some(Action::Plugin {
        id,
        plugin,
        args,
        comments,
        summary,
    }) = detect
    {
        session.plugin(id, &plugin, args, comments.as_deref(), summary);
    }
    let result = session.interact(&mut app, tx, &rx);
    term::restore();
    result?;
    Ok(ExitCode::SUCCESS)
}

/// Runs the viewer on `tty` for the diff arriving on `stdin`, read only as
/// far ahead of the user as [`piped::READ_AHEAD`] entries. Nothing waits
/// for the end of the input.
pub(crate) fn run_piped(tty: &Tty, stdin: Stdin, ctx: &Ctx) -> Result<ExitCode, Error> {
    let (tx, rx) = mpsc::channel();
    let settings = worker::Settings {
        options: Options::default(),
        highlight: false,
        grammars: crate::grammars(),
        cache: None,
    };
    let pool = worker::Pool::new(None, &settings, &tx)?;
    let gate = Arc::new(piped::Gate::new(piped::READ_AHEAD));
    piped::feed(stdin, tx.clone(), Arc::clone(&gate))?;
    let first = loop {
        match rx.recv() {
            Ok(Event::Piped(item)) => break item,
            Ok(Event::EnumerationDone(Some(error))) => return Err(Error::fatal(error)),
            Ok(Event::EnumerationDone(None)) | Err(_) => return Ok(ExitCode::SUCCESS),
            Ok(_) => {}
        }
    };
    let mut app = App::new(
        &ctx.config,
        theme(ctx),
        "stdin".to_owned(),
        None,
        tty.size(),
        ctx.indicators(),
    );
    app.set_piped();
    prepare(&mut app, ctx);
    app.event(Event::Piped(first));
    state::flush_on_panic();
    term::install_panic_hook();
    let mut session = Session {
        watching: None,
        review_run: None,
        tty,
        reader: None,
        pool: &pool,
        suspended: Arc::new(AtomicBool::new(false)),
        files: None,
        root: None,
        compared: Vec::new(),
        local: Vec::new(),
        requests: mpsc::channel().0,
        events: tx.clone(),
        watch: None,
        tmux_clipboard: std::cell::OnceCell::new(),
        gate: Some(gate),
    };
    let result = session.interact(&mut app, tx, &rx);
    term::restore();
    result?;
    Ok(ExitCode::SUCCESS)
}

/// Sends [`Event::Comments`] when one of `paths` is written, created or
/// replaced, dffy's own writes included, until the returned handle is
/// dropped or the loop is gone. Each file's directory is watched, created first if
/// missing as writing the file would, so a write through a temporary file
/// and a rename shows; the watcher's debounce makes a burst of writes one
/// event. A file that can't be watched goes without: its reloads then come
/// from dffy's own writes and review plugin runs only.
///
/// The watches start on a thread of their own, as starting one waits on
/// the OS, and an [`Event::Comments`] follows once they're in place: read
/// the files before calling this, and a write in between isn't missed.
fn watch_comments(
    paths: Vec<PathBuf>,
    events: mpsc::Sender<Event>,
) -> Option<JoinHandle<Vec<Watcher>>> {
    let start = move || {
        let watchers = paths
            .iter()
            .filter_map(|path| {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).ok();
                }
                let events = events.clone();
                Watcher::start(Root::file(path).ok()?, move |notice| match notice {
                    Notice::Changed(_) | Notice::Rescan => events.send(Event::Comments).is_ok(),
                    Notice::Polling(_) => true,
                })
                .ok()
            })
            .collect();
        events.send(Event::Comments).ok();
        watchers
    };
    std::thread::Builder::new()
        .name("dffy-comments-watch".into())
        .spawn(start)
        .ok()
}

/// Opens `url` with the platform's opener, detached from the terminal;
/// says so when it can't start.
fn browse(app: &mut App, url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let started = Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match started {
        // Reaped off the loop: openers return at once, but a slow one
        // shouldn't hold a frame.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
            app.message("opened in the browser");
        }
        Err(e) => app.message(format!("couldn't run {opener}: {e}")),
    }
}

/// Writes a review's summary to a new file of its own, for a plugin's
/// `--summary-file`.
fn write_summary(text: &str) -> io::Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("dffy-summary-{}.md", std::process::id()));
    // Left over from a run that crashed; `create_new` then refuses to
    // follow a link put in its place.
    std::fs::remove_file(&path).ok();
    std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut file| file.write_all(text.as_bytes()))?;
    Ok(path)
}

/// The watch's sink: changes go to the change-set thread, the watch's
/// state to the loop. Returns false once either is gone.
fn route(
    root: &Root,
    requests: &mpsc::Sender<Request>,
    events: &mpsc::Sender<Event>,
    event: client::Event,
) -> bool {
    match event {
        client::Event::Notice(Notice::Changed(paths)) => {
            let mut worktree = Vec::new();
            let mut git = false;
            for path in &paths {
                match root.classify(path) {
                    Some(Change::Worktree(rel)) => worktree.push(rel),
                    Some(Change::Git) => git = true,
                    None => {}
                }
            }
            let request = if git {
                Request::Rescan
            } else if worktree.is_empty() {
                return true;
            } else {
                Request::Refresh(worktree)
            };
            requests.send(request).is_ok()
        }
        client::Event::Notice(Notice::Rescan) => requests.send(Request::Rescan).is_ok(),
        // Whatever changed before the watch was ready went unseen.
        client::Event::Ready { .. } => {
            requests.send(Request::Rescan).is_ok() && events.send(Event::Watch(event)).is_ok()
        }
        event => events.send(Event::Watch(event)).is_ok(),
    }
}

/// [`route`] for a path compared outside git: changes in a folder refresh
/// the same relative paths on both sides; a file's re-lists its one entry.
fn route_compared(
    root: &Root,
    requests: &mpsc::Sender<Request>,
    events: &mpsc::Sender<Event>,
    notice: Notice,
) -> bool {
    let request = match notice {
        Notice::Changed(paths) => {
            let changed: Vec<BString> = paths
                .iter()
                .filter_map(|path| match root.classify(path) {
                    Some(Change::Worktree(rel)) => Some(rel),
                    _ => None,
                })
                .collect();
            if changed.is_empty() {
                return true;
            }
            if matches!(root.plain, Some(Plain::File(_))) {
                Request::Rescan
            } else {
                Request::Refresh(changed)
            }
        }
        Notice::Rescan => Request::Rescan,
        notice @ Notice::Polling(_) => {
            return events
                .send(Event::Watch(client::Event::Notice(notice)))
                .is_ok();
        }
    };
    requests.send(request).is_ok()
}

struct Session<'a> {
    /// The comments files' watches, one per file, as the thread starting
    /// them returns them; see [`watch_comments`]. Never joined: dropping the
    /// handle drops the watches once the thread ends, which stops them.
    watching: Option<JoinHandle<Vec<Watcher>>>,
    /// The review plugin's run other than a check, the last one started.
    review_run: Option<review::Run>,
    tty: &'a Tty,
    reader: Option<Reader>,
    pool: &'a worker::Pool,
    suspended: Arc<AtomicBool>,
    /// Where the files `e` opens are; `None` for a piped diff.
    files: Option<Repo>,
    /// `None` without a working tree to watch.
    root: Option<Root>,
    /// The paths compared outside git that are on disk, watched instead of
    /// `root`.
    compared: Vec<Root>,
    /// Their watchers, in-process: the broker shares watches by
    /// repository, and these aren't in one.
    local: Vec<Watcher>,
    requests: mpsc::Sender<Request>,
    events: mpsc::Sender<Event>,
    watch: Option<Subscription>,
    /// tmux's `set-clipboard`, asked at the first copy: `None` outside tmux
    /// or when it can't be read.
    tmux_clipboard: std::cell::OnceCell<Option<String>>,
    /// How far a piped diff's parser may read.
    gate: Option<Arc<piped::Gate>>,
}

impl Session<'_> {
    /// Takes over the terminal and runs the event loop; the caller restores
    /// the terminal however this ends.
    fn interact(
        &mut self,
        app: &mut App,
        tx: mpsc::Sender<Event>,
        rx: &mpsc::Receiver<Event>,
    ) -> io::Result<()> {
        self.tty.enter()?;
        if !app.mouse() {
            self.tty.write(term::MOUSE_OFF)?;
        }
        // Asked before the first frame, never waited for: the reply comes
        // in as input, or never.
        if app.wants_background() {
            self.tty.write(term::QUERY_BACKGROUND)?;
            app.asked_background();
        }
        let input = tx.clone();
        self.reader =
            Some(self.tty.spawn_reader(move |i| {
                input.send(i.map_or(Event::Interrupt, Event::Input)).is_ok()
            })?);
        // An editor started with `e` shares the process group, so its Ctrl-C
        // reaches dffy too; it's the editor's.
        let signals = Arc::clone(&self.suspended);
        term::on_signals(move |signal| match signal {
            Signal::Resize => tx.send(Event::Resize).is_ok(),
            Signal::Interrupt if signals.load(Ordering::Relaxed) => true,
            Signal::Interrupt => tx.send(Event::Interrupt).is_ok(),
        })?;
        let result = self.run(app, rx);
        crate::plugin::stop_all();
        let remembered = app.take_remembered(true);
        if !remembered.is_empty() {
            state::write(&remembered);
        }
        state::stage(&[]);
        result
    }

    fn set_watch(&mut self, app: &mut App, on: bool) {
        self.watch = None;
        self.local.clear();
        if !on {
            return;
        }
        if !self.compared.is_empty() {
            self.watch_compared(app);
            return;
        }
        let Some(root) = self.root.clone() else {
            app.set_watching(false);
            app.message("there's no working tree to watch");
            return;
        };
        let (requests, events, routing) =
            (self.requests.clone(), self.events.clone(), root.clone());
        match client::subscribe(root, move |event| {
            route(&routing, &requests, &events, event)
        }) {
            Ok(subscription) => self.watch = Some(subscription),
            Err(e) => {
                app.set_watching(false);
                app.message(format!("couldn't watch: {e}"));
            }
        }
    }

    fn watch_compared(&mut self, app: &mut App) {
        let mut count = 0;
        let mut backend = "";
        for root in &self.compared {
            let (requests, events, routing) =
                (self.requests.clone(), self.events.clone(), root.clone());
            match Watcher::start(root.clone(), move |notice| {
                route_compared(&routing, &requests, &events, notice)
            }) {
                Ok(watcher) => {
                    count += watcher.watches();
                    backend = watcher.backend();
                    self.local.push(watcher);
                }
                Err(e) => {
                    self.local.clear();
                    app.set_watching(false);
                    app.message(format!("couldn't watch: {e}"));
                    return;
                }
            }
        }
        let ready = client::Event::Ready {
            backend: backend.to_owned(),
            watches: u32::try_from(count).unwrap_or(u32::MAX),
        };
        // Whatever changed before the watches were in place went unseen.
        self.requests.send(Request::Rescan).ok();
        self.events.send(Event::Watch(ready)).ok();
    }

    fn run(&mut self, app: &mut App, rx: &mpsc::Receiver<Event>) -> io::Result<()> {
        loop {
            if let Some(frame) = app.draw() {
                self.tty.write(frame)?;
                self.pool.replace(app.jobs());
                if let Some(gate) = &self.gate {
                    gate.want(app.wanted());
                }
            }
            if let Some(files) = app.take_count_pass() {
                self.pool.count(files);
            }
            let languages = app.take_languages();
            if !languages.is_empty() {
                self.pool.prepare(languages);
            }
            let remembered = app.take_remembered(false);
            if !remembered.is_empty() {
                state::write(&remembered);
            }
            state::stage(app.pending_remembered());
            let first = match app.deadline() {
                Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(event) => event,
                    Err(mpsc::RecvTimeoutError::Timeout) => Event::Tick,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                },
                None => match rx.recv() {
                    Ok(event) => event,
                    Err(_) => return Ok(()),
                },
            };
            let started = Instant::now();
            let mut next = Some(first);
            while let Some(event) = next {
                if let Event::Diffed(done) = &event {
                    self.pool.finish(done);
                }
                match event {
                    Event::Resize => {
                        let (width, height) = self.tty.size();
                        app.resize(width, height);
                    }
                    Event::Interrupt => return Ok(()),
                    event => match app.event(event) {
                        Some(Action::Quit) => return Ok(()),
                        Some(Action::Edit { path, line }) => self.edit(app, &path, line)?,
                        Some(Action::Compose(text)) => self.compose(app, &text)?,
                        Some(Action::Browse(url)) => browse(app, &url),
                        Some(Action::Plugin {
                            id,
                            plugin,
                            args,
                            comments,
                            summary,
                        }) => self.plugin(id, &plugin, args, comments.as_deref(), summary),
                        Some(Action::Retarget {
                            revisions,
                            comments,
                            moved,
                        }) => self.retarget(app, revisions, comments, moved),
                        Some(Action::Watch(on)) => self.set_watch(app, on),
                        Some(Action::Mouse(on)) => {
                            self.tty
                                .write(if on { term::MOUSE_ON } else { term::MOUSE_OFF })?;
                        }
                        Some(Action::Rediff {
                            highlight,
                            whitespace,
                        }) => self.pool.set(highlight, whitespace),
                        Some(Action::Copy { text, note }) => self.copy(app, &text, note)?,
                        Some(Action::CopyFullPath(path)) => self.copy_full_path(app, &path)?,
                        None => {}
                    },
                }
                next = if started.elapsed() < BATCH_TIME {
                    rx.try_recv().ok()
                } else {
                    None
                };
            }
        }
    }

    /// Puts `text` on the clipboard, then says `note`.
    fn copy(&self, app: &mut App, text: &[u8], mut note: String) -> io::Result<()> {
        self.tty.write(&term::clipboard(text))?;
        let tmux = self.tmux_clipboard.get_or_init(term::tmux_clipboard);
        if tmux.as_ref().is_some_and(|value| value != "on") {
            note.push_str("; tmux needs set-clipboard on");
        }
        app.message(note);
        Ok(())
    }

    /// Puts where the repository-relative `path` is on disk on the
    /// clipboard.
    fn copy_full_path(&self, app: &mut App, path: &BString) -> io::Result<()> {
        let Some(full) = self.files.as_ref().and_then(|f| f.on_disk(path.as_bstr())) else {
            app.message(if self.files.is_some() {
                "this file isn't on disk"
            } else {
                "there's no working tree for this file to be in"
            });
            return Ok(());
        };
        // The working tree is as dffy was pointed at it, maybe relative.
        let full = std::path::absolute(&full).unwrap_or(full);
        let bytes = full.as_os_str().as_bytes();
        self.copy(app, bytes, format!("copied {}", escape(bytes)))
    }

    /// Hands the terminal to the editor and takes it back.
    fn edit(&self, app: &mut App, path: &BString, line: usize) -> io::Result<()> {
        let (Some(full), Some(reader)) = (
            self.files.as_ref().and_then(|f| f.on_disk(path.as_bstr())),
            &self.reader,
        ) else {
            // Stdin, a bare repository, or a path behind a symbolic link to
            // a directory, which leads out of the working tree.
            app.message(if self.files.is_some() {
                "this file isn't on disk to open"
            } else {
                "there's no working tree to open files in"
            });
            return Ok(());
        };
        self.in_editor(app, reader, &full, line)?;
        Ok(())
    }

    /// Hands the terminal to the program `run` starts, given the terminal
    /// for its input, and takes it back once `run` returns. The outer
    /// error is the terminal's, the inner one `run`'s.
    fn hand_over(
        &self,
        app: &mut App,
        reader: &Reader,
        run: impl FnOnce(std::fs::File) -> io::Result<ExitStatus>,
    ) -> io::Result<io::Result<ExitStatus>> {
        reader.pause();
        self.suspended.store(true, Ordering::Relaxed);
        term::restore();
        let status = self.tty.try_clone().and_then(run);
        self.tty.enter()?;
        if !app.mouse() {
            self.tty.write(term::MOUSE_OFF)?;
        }
        self.suspended.store(false, Ordering::Relaxed);
        reader.resume();
        let (width, height) = self.tty.size();
        app.resize(width, height);
        app.invalidate();
        Ok(status)
    }

    /// Hands the terminal to the editor on `path` at `line` and takes it
    /// back. Returns whether the editor exited cleanly; why not goes in
    /// the status bar.
    fn in_editor(
        &self,
        app: &mut App,
        reader: &Reader,
        path: &Path,
        line: usize,
    ) -> io::Result<bool> {
        let status = self.hand_over(app, reader, |tty| editor::open(path, line, tty))?;
        Ok(match status {
            Ok(status) if status.success() => true,
            Ok(status) => {
                app.message(format!("the editor exited with {status}"));
                false
            }
            Err(e) => {
                app.message(format!("couldn't start the editor: {e}"));
                false
            }
        })
    }

    /// Starts `dffy-review-<plugin> <args>` in the background, in the
    /// working tree's root, as run `id`; a summary goes in a file of its
    /// own, removed once the run is over. A `prepare`, `pull` or `post`
    /// kills the one before it. A run that can't start says why as its end.
    fn plugin(
        &mut self,
        id: u64,
        plugin: &str,
        mut args: Vec<String>,
        comments: Option<&Path>,
        summary: Option<String>,
    ) {
        let name = format!("{}{plugin}", crate::plugin::REVIEW_PREFIX);
        let events = self.events.clone();
        let fail = |why: String| {
            events
                .send(Event::Review {
                    id,
                    said: Said::Done(Err(why)),
                })
                .ok();
        };
        let Some(root) = self.files.as_ref().and_then(Repo::workdir) else {
            return fail("there's no working tree to run the review plugin in".to_owned());
        };
        let Some(program) = crate::plugin::find(crate::plugin::REVIEW_PREFIX, plugin) else {
            return fail(format!("{name} isn't on PATH (see docs/plugins.md)"));
        };
        let summary = match summary.map(|text| write_summary(&text)).transpose() {
            Ok(file) => file,
            Err(e) => return fail(format!("couldn't write the summary: {e}")),
        };
        if let Some(file) = &summary {
            args.extend(["--summary-file".to_owned(), file.display().to_string()]);
        }
        let verb = args.first().cloned().unwrap_or_default();
        // `check`, `list`, `detect` and `post --dry-run` run beside these.
        let one_at_a_time = matches!(verb.as_str(), "prepare" | "pull" | "post")
            && !args.iter().any(|a| a == "--dry-run");
        if one_at_a_time && let Some(old) = self.review_run.take() {
            old.kill();
        }
        let sink = self.events.clone();
        let started = review::command(&program.path, &args, root, comments).and_then(|command| {
            review::start(command, &name, &verb, move |said| {
                if let (Said::Done(_), Some(file)) = (&said, &summary) {
                    std::fs::remove_file(file).ok();
                }
                sink.send(Event::Review { id, said }).is_ok()
            })
        });
        match started {
            Ok(run) if one_at_a_time => self.review_run = Some(run),
            Ok(_) => {}
            Err(e) => fail(format!("couldn't start {name}: {e}")),
        }
    }

    /// Lists `revisions` from now on, when given, with the comments in
    /// `comments` when that's another file. `moved` is a review's head before
    /// and after an update, whose commits are counted for the note.
    fn retarget(
        &mut self,
        app: &mut App,
        revisions: Option<Revisions>,
        comments: Option<PathBuf>,
        moved: Option<(String, String)>,
    ) {
        if let Some(mut revisions) = revisions {
            // Checked first: once the list has a range that doesn't
            // resolve, every refresh after fails too.
            if let (Some(last), Some(repo)) = (app.reviewed_from(), &self.files)
                && !repo.is_revision(last)
                && let Some(whole) = app.reviewed_head_missing()
            {
                revisions = whole;
            }
            self.requests.send(Request::Retarget(revisions)).ok();
        }
        if let Some(path) = comments {
            let files = app.comment_files(path.clone());
            app.set_comments(path);
            self.watching = watch_comments(files, self.events.clone());
        }
        if let (Some((old, new)), Some(root)) = (moved, self.files.as_ref().and_then(Repo::workdir))
        {
            let count = Command::new("git")
                .args(["rev-list", "--count", &format!("{old}..{new}")])
                .current_dir(root)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|out| out.status.success())
                .and_then(|out| String::from_utf8(out.stdout).ok()?.trim().parse().ok());
            app.review_updated(count);
        }
    }

    /// Writes a comment in the editor, from `text`, on a temporary file
    /// removed after; what's written goes to [`App::composed`].
    fn compose(&self, app: &mut App, text: &str) -> io::Result<()> {
        let Some(reader) = &self.reader else {
            app.composed(None);
            return Ok(());
        };
        let path = std::env::temp_dir().join(format!("dffy-comment-{}.md", std::process::id()));
        // Left over from a run that crashed; `create_new` then refuses to
        // follow a link put in its place.
        std::fs::remove_file(&path).ok();
        let created = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&path)
            .and_then(|mut file| file.write_all(text.as_bytes()));
        if let Err(e) = created {
            app.message(format!("couldn't write {}: {e}", path.display()));
            app.composed(None);
            return Ok(());
        }
        let written = if self.in_editor(app, reader, &path, 1)? {
            std::fs::read_to_string(&path).ok()
        } else {
            None
        };
        std::fs::remove_file(&path).ok();
        app.composed(written);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The comments files are watched, even one whose directory doesn't
    /// exist yet, and an event says the watches are in place: creating one,
    /// replacing it through a rename, or writing the other then sends an
    /// event, and dropping the handle, never joined, ends the watches. How
    /// many events a write makes is the backend's; the app's reload skips
    /// those that changed nothing.
    #[test]
    fn a_change_to_the_comments_file_sends_an_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dffy").join("comments.json");
        let local = dir.path().join("local.json");
        let (tx, rx) = mpsc::channel();
        let watching = watch_comments(vec![path.clone(), local.clone()], tx).expect("spawned");
        let wait = Duration::from_secs(10);
        assert!(
            matches!(rx.recv_timeout(wait), Ok(Event::Comments)),
            "in place"
        );
        let empty = "{\"version\":1,\"comments\":[]}";
        std::fs::write(&path, empty).expect("write");
        assert!(matches!(rx.recv_timeout(wait), Ok(Event::Comments)));
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, "{\"version\":1,\"notice\":\"x\",\"comments\":[]}").expect("write");
        std::fs::rename(&tmp, &path).expect("rename");
        assert!(matches!(rx.recv_timeout(wait), Ok(Event::Comments)));
        std::fs::write(&local, empty).expect("write");
        assert!(matches!(rx.recv_timeout(wait), Ok(Event::Comments)));
        drop(watching);
        loop {
            match rx.recv_timeout(wait) {
                Ok(Event::Comments) => {}
                Ok(_) => panic!("not a comments event"),
                Err(e) => {
                    assert_eq!(e, mpsc::RecvTimeoutError::Disconnected, "the watches end");
                    break;
                }
            }
        }
    }
}

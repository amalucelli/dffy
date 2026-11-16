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
mod editor;
mod fuzzy;
mod help;
mod picker;
pub(crate) mod piped;
mod tree;
pub(crate) mod view;
pub(crate) mod worker;

use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use dffy_core::bstr::{BString, ByteSlice};
use dffy_core::diff::Options;
use dffy_core::git::{ChangeSet, FileChange, Repo, Revisions, Update};
use dffy_core::watch::{Change, Notice, Plain, Root, Watcher};

pub(crate) use app::{Action, App};
pub(crate) use view::{Counts, Loaded};
pub(crate) use worker::Done;

use crate::daemon::client::{self, Subscription};
use crate::pager::stdin::Stdin;
use crate::print::{self, Plan};
use crate::render::Canvas;
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
    app.set_view_remembered(ctx.sources.get("view") == crate::config::Source::LastUsed);
    app.start_tree(ctx.sources.get("tree") == crate::config::Source::LastUsed);
    app.set_context_lines(ctx.context_lines);
    if ctx.config.tips && !ctx.tip_shown {
        app.show_tip();
        state::write(&[(state::TIP_SHOWN, "true".to_owned())]);
    }
}

/// Runs the viewer on `tty` for `plan` until the user quits. An empty diff
/// prints [`print::empty_message`] instead, without taking over the screen,
/// unless watching: then the viewer waits for changes.
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
                if watch || commit.is_some() {
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

    state::flush_on_panic();
    crate::plugin::stop_on_panic();
    term::install_panic_hook();
    let mut session = Session {
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
                        Some(Action::Watch(on)) => self.set_watch(app, on),
                        Some(Action::Mouse(on)) => {
                            self.tty
                                .write(if on { term::MOUSE_ON } else { term::MOUSE_OFF })?;
                        }
                        Some(Action::Rediff {
                            highlight,
                            whitespace,
                        }) => self.pool.set(highlight, whitespace),
                        Some(Action::Copy { text, mut note }) => {
                            self.tty.write(&term::clipboard(&text))?;
                            let tmux = self.tmux_clipboard.get_or_init(term::tmux_clipboard);
                            if tmux.as_ref().is_some_and(|value| value != "on") {
                                note.push_str("; tmux needs set-clipboard on");
                            }
                            app.message(note);
                        }
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
        reader.pause();
        self.suspended.store(true, Ordering::Relaxed);
        term::restore();
        let status = self
            .tty
            .try_clone()
            .and_then(|tty| editor::open(&full, line, tty));
        self.tty.enter()?;
        if !app.mouse() {
            self.tty.write(term::MOUSE_OFF)?;
        }
        self.suspended.store(false, Ordering::Relaxed);
        reader.resume();
        let (width, height) = self.tty.size();
        app.resize(width, height);
        app.invalidate();
        match status {
            Ok(status) if status.success() => {}
            Ok(status) => app.message(format!("the editor exited with {status}")),
            Err(e) => app.message(format!("couldn't start the editor: {e}")),
        }
        Ok(())
    }
}

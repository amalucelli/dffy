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
mod markdown;
mod menu;
mod picker;
pub(crate) mod piped;
mod textbox;
mod tree;
pub(crate) mod view;
pub(crate) mod worker;

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dffy_core::bstr::{BString, ByteSlice};
use dffy_core::conflict::Choices;
use dffy_core::diff::Options;
use dffy_core::git::{
    Applied, ChangeSet, DiffSpec, Edit, FileChange, FileDiff, Picked, Repo, Revisions, Update,
};
use dffy_core::watch::{Change, Notice, Plain, Root, Watcher};

pub(crate) use app::{Action, App, Uncommitted};
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
use dffy_core::cache::Cache;
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
    /// The branch comments are written on now, as read when git's state
    /// changed: HEAD moved to another, or detached (`None`).
    Branch(Option<String>),
    /// Lines were staged, unstaged or discarded, or a file added, as the
    /// note says, or why not.
    Edited(Result<String, String>),
    /// What discarding `picked` lines of `diff` would take, or why it
    /// can't.
    Planned {
        diff: Arc<FileDiff>,
        picked: Picked,
        result: Result<Applied, String>,
    },
    /// The conflicted file `path` was written as its choices resolve it,
    /// with how many conflicts kept their markers, or why not.
    Resolved {
        path: BString,
        result: Result<usize, String>,
    },
    /// The review plugin's request `id` said this.
    Review {
        id: u64,
        said: Said,
    },
    /// The review plugin said this on its own.
    ReviewPush(review::Push),
    /// How many commits a review's update moved its head by, when git
    /// could tell.
    ReviewMoved(Option<usize>),
    /// Git says the head of your last review, `last`, isn't an ancestor of
    /// the review's `head`, as after a rebase.
    ReviewRebased {
        last: String,
        head: String,
    },
    /// The review plugin's naming, asked for after the first frame; `None`
    /// when it can't initialize.
    Described(Option<review::Described>),
    /// A request from the control API, or what's subscribed to now.
    Control(crate::control::viewer::Inbound),
    /// Nothing happened before the app's deadline.
    Tick,
}

/// Events handled in one go before drawing, at most; a burst (thousands
/// of files from the enumeration) still gets frames out.
const BATCH_TIME: Duration = Duration::from_millis(8);

/// Why a git operation on the working tree can't run.
const NO_WORKTREE: &str = "there's no working tree to change";

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

/// How the review plugin's command (`dffy pr`) starts the viewer.
pub(crate) enum ReviewStart {
    /// On the review whose `prepare`, a request of `client`'s, named
    /// `session` and goes on as `run`, saying the rest on `rest`.
    Open {
        plugin: String,
        client: review::Plugin,
        session: Box<review::Session>,
        run: review::Run,
        rest: mpsc::Receiver<Said>,
    },
    /// With the picker of `reviews` open, for a branch without a review.
    Pick {
        plugin: String,
        client: review::Plugin,
        reviews: Vec<Listed>,
    },
    /// Plain `dffy` on a branch: the review plugin, once it has initialized,
    /// looks for the branch's review, whose comments and status join what
    /// the viewer shows, if there's one.
    Detect,
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
    // A view of a working tree registers for the control API; one of paths
    // compared outside git has no repository to be found by.
    let register = repo
        .workdir()
        .zip(repo.common_dir())
        .map(|(worktree, shared)| crate::control::viewer::Info {
            worktree: worktree.to_owned(),
            repository: shared.to_owned(),
            diff: description.clone(),
        });
    let uncommitted = uncommitted(&plan.spec, &revisions, plan.reverse);
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
                    .is_some_and(|p| !matches!(p, ReviewStart::Detect));
                if watch || commit.is_some() || opened {
                    break None;
                }
                let mut c = Canvas::default();
                let mut message =
                    print::empty_message(&repo, &selected, &paths, ctx.config.default_diff);
                message.extend(open_comments(&repo));
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
    // Not in `prepare`: a piped diff has no review, so the plugin isn't
    // asked to describe itself for one. Unless parsing needed the
    // description, it's asked for once the first frame is out, as an
    // uncached `describe` runs the plugin.
    let described = ctx.review.get();
    if let Some(described) = described {
        app.set_naming(described.as_ref().ok().map(|d| d.naming.clone()));
    }
    app.set_selected(selected, ctx.config.default_diff);
    app.set_uncommitted(uncommitted);
    app.set_repository(
        repo.workdir()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned()),
    );
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
    let unread = Unread {
        given: ctx.comments.clone(),
        review: matches!(start, Some(ReviewStart::Open { .. })),
    };
    let mut review_run = None;
    let mut detect = None;
    let mut detect_described = false;
    let mut client = None;
    match start {
        Some(ReviewStart::Detect) => match described {
            Some(Ok(d)) => detect = Some(app.detect_review(d.program.name.clone())),
            Some(Err(_)) => {}
            None => detect_described = true,
        },
        Some(ReviewStart::Open {
            plugin,
            client: running,
            session,
            run,
            rest,
        }) => {
            running.watch(Some(&session.id));
            client = Some(running);
            let id = app.start_review(plugin, *session);
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
        Some(ReviewStart::Pick {
            plugin,
            client: running,
            reviews,
        }) => {
            client = Some(running);
            app.pick_review(plugin, reviews);
        }
        None => {}
    }
    if let Some(client) = &client {
        let events = tx.clone();
        client.on_push(move |push| events.send(Event::ReviewPush(push)).is_ok());
    }

    state::flush_on_panic();
    crate::plugin::stop_on_panic();
    term::install_panic_hook();
    let mut session = Session {
        watching: None,
        unread: Some(unread),
        describe: described.is_none() && ctx.review_name.is_some(),
        detect,
        detect_described,
        review_run,
        plugin: client,
        review_name: ctx.review_name.clone(),
        cache: ctx.cache.clone(),
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
        register,
        control: None,
    };
    if watch {
        app.set_watching(true);
        session.set_watch(&mut app, true);
    }
    let result = session.interact(&mut app, tx, &rx);
    term::restore();
    // Off the alternate screen, so it stays once the viewer is gone.
    if let Some(note) = app.unposted_note() {
        eprintln!("{}", escape(note.as_bytes()));
    }
    result?;
    Ok(ExitCode::SUCCESS)
}

/// What an empty diff's message adds when the local review's file has
/// threads left open on this branch, and how to see them: read only on
/// the way out, with no frame waiting on it.
fn open_comments(repo: &Repo) -> Option<String> {
    let file = crate::review::local_file_read_only(repo)?;
    let comments = crate::review::read(&file).ok()?;
    let branch = crate::review::current_branch(repo);
    let select = crate::review::Select {
        resolved: Some(false),
        branches: crate::review::Branches::On(branch.as_deref()),
        ..crate::review::Select::default()
    };
    let open = (comments.selected(&select).iter())
        .filter(|c| c.in_reply_to.is_none() && !c.path.is_empty())
        .count();
    let s = if open == 1 { "" } else { "s" };
    (open > 0).then(|| {
        format!(" · {open} open comment{s} · dffy comments lists them, dffy HEAD~1.. shows them")
    })
}

/// Which uncommitted changes `spec` lists, if it lists them as plain `dffy`,
/// `--staged` or `--unstaged` do: the diffs whose lines can be staged.
fn uncommitted(spec: &DiffSpec, revisions: &Revisions, reverse: bool) -> Option<Uncommitted> {
    if reverse || !revisions.revs.is_empty() || revisions.merge_base {
        return None;
    }
    match spec {
        DiffSpec::Uncommitted { .. } => Some(Uncommitted::Plain),
        DiffSpec::IndexToWorktree { .. } => Some(Uncommitted::Unstaged),
        DiffSpec::TreeToIndex { .. } => Some(Uncommitted::Staged),
        DiffSpec::TreeToWorktree { .. } | DiffSpec::TreeToTree { .. } | DiffSpec::Paths => None,
    }
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
        unread: None,
        describe: false,
        detect: None,
        detect_described: false,
        review_run: None,
        plugin: None,
        review_name: None,
        cache: None,
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
        register: None,
        control: None,
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
    std::thread::Builder::new()
        .name("dffy-comments-watch".into())
        .spawn(move || {
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
        })
        .ok()
}

/// How long a signal to stop waits for a post running to end, as killing
/// it leaves the review half sent until the next post.
const POST_GRACE: Duration = Duration::from_secs(30);

/// A signal or the terminal going away stops the viewer: a post running
/// is cancelled, as the second `q` does, and may end first (see
/// [`finish_post`]); then the text box's text and the post dialog's
/// summary are saved. The terminal may be gone, so nothing is drawn.
fn stop(app: &mut App, run: Option<&review::Run>, rx: &mpsc::Receiver<Event>) {
    if app.posting_now()
        && let Some(run) = run
    {
        run.cancel();
    }
    finish_post(app, rx);
    app.interrupted();
}

/// Waits for the post running, if any, to end, for up to [`POST_GRACE`],
/// taking only what review plugins say: its refs are written as it ends.
fn finish_post(app: &mut App, rx: &mpsc::Receiver<Event>) {
    let until = Instant::now() + POST_GRACE;
    while app.posting_now() {
        match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(event @ (Event::Review { .. } | Event::Comments)) => {
                app.event(event);
            }
            Ok(_) => {}
            Err(_) => return,
        }
    }
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

/// Writes `text` to a new file at `path` that only you can read.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    // Left over from a run that crashed; `create_new` then refuses to
    // follow a link put in its place.
    std::fs::remove_file(path).ok();
    std::fs::File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
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

/// What the viewer's comments files are found from.
struct Unread {
    /// `--comments`.
    given: Option<PathBuf>,
    /// The viewer opens on a review, whose file it reads instead, the
    /// local review's showing its drafts on it.
    review: bool,
}

struct Session<'a> {
    /// The comments files' watches, one per file, as the thread starting
    /// them returns them; see [`watch_comments`]. Never joined: dropping the
    /// handle drops the watches once the thread ends, which stops them.
    watching: Option<JoinHandle<Vec<Watcher>>>,
    /// What the comments files are found from, found, read and watched
    /// only once the first frame is out: finding the local review's file
    /// may move it, and its parse and the watch thread aren't the first
    /// frame's either.
    unread: Option<Unread>,
    /// The review plugin's naming is still to be asked for, once the first
    /// frame is out; see [`Event::Described`].
    describe: bool,
    /// The `detect` request for the branch's review, made once the first
    /// frame is out: starting the plugin takes a fork and exec.
    detect: Option<Action>,
    /// Plain `dffy` on a branch waits for the plugin to initialize to
    /// `detect`: a plugin that can't isn't asked.
    detect_described: bool,
    /// The last `prepare`, `pull` or `post` made.
    review_run: Option<review::Run>,
    /// The review plugin's process, started on first use, for the viewer's
    /// life.
    plugin: Option<review::Plugin>,
    /// The review plugin set, and the cache its naming goes in.
    review_name: Option<String>,
    cache: Option<Cache>,
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
    /// What the view registers as for the control API, once the first
    /// frame is out; `None` once it did, and for a view that doesn't.
    register: Option<crate::control::viewer::Info>,
    /// The registration, for the viewer's life.
    control: Option<crate::control::viewer::Registration>,
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
        if let Some(plugin) = &self.plugin {
            plugin.close();
        }
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

    /// Starts what waits for a frame to be out: the review plugin's
    /// naming, the comments file and its watch, `detect`, and the control
    /// API's registration.
    fn start_deferred(&mut self, app: &mut App) -> io::Result<()> {
        if let Some(info) = self.register.take() {
            let events = self.events.clone();
            let registered = crate::control::viewer::register(info, move |inbound| {
                events.send(Event::Control(inbound)).is_ok()
            });
            match registered {
                Ok(registration) => self.control = Some(registration),
                Err(e) => app.message(format!("dffy ctl can't reach this viewer: {e}")),
            }
        }
        if std::mem::take(&mut self.describe) {
            // `detect` needs the plugin running anyway, so it's the one
            // asked; otherwise the cache, else a plugin started for it
            // alone, answers.
            if self.detect_described {
                self.plugin().ok();
            } else if let Some(name) = self.review_name.clone() {
                let (events, cache) = (self.events.clone(), self.cache.clone());
                std::thread::Builder::new()
                    .name("dffy-review-describe".into())
                    .spawn(move || {
                        let described = crate::describe_review(&name, cache.as_ref(), false);
                        events.send(Event::Described(described.ok())).ok();
                    })?;
            }
        }
        if let Some(unread) = self.unread.take() {
            // A local file that can't be had, as when moving an old one
            // failed, leaves the viewer without it rather than without the
            // diff.
            let default = match self.files.as_ref().map(crate::review::local_file) {
                Some(Ok(file)) => file,
                Some(Err(e)) => {
                    app.message(format!("no local comments: {e}"));
                    None
                }
                None => None,
            };
            // Absolute: review plugins get it, and run from the working
            // tree's root.
            let absolute = |path: PathBuf| std::path::absolute(&path).unwrap_or(path);
            let comments = unread.given.or_else(|| default.clone()).map(absolute);
            app.set_local_comments(if unread.review {
                default.map(absolute)
            } else {
                comments.clone()
            });
            if let Some(path) = comments {
                // HEAD is read here, after the first frame, once.
                app.set_branch(self.files.as_ref().and_then(crate::review::current_branch));
                let files = app.comment_files(path.clone());
                app.set_comments(path);
                self.watching = watch_comments(files, self.events.clone());
                self.point_session(app);
            }
        }
        if let Some(Action::Review { id, request }) = self.detect.take() {
            self.request(id, &request);
        }
        Ok(())
    }

    fn run(&mut self, app: &mut App, rx: &mpsc::Receiver<Event>) -> io::Result<()> {
        loop {
            self.paint(app)?;
            if self.unread.is_some()
                || self.describe
                || self.detect.is_some()
                || self.register.is_some()
            {
                self.start_deferred(app)?;
                // The threads they placed and the jobs they want go out
                // before the loop waits.
                continue;
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
            if let (Some(idle), Some(plugin)) = (app.take_idle(), &self.plugin) {
                plugin.idle(idle);
            }
            if let Some(text) = app.take_orphaned() {
                keep_text(app, &text);
            }
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
                    Event::Interrupt => {
                        stop(app, self.review_run.as_ref(), rx);
                        return Ok(());
                    }
                    Event::Described(Some(described)) => {
                        app.set_naming(Some(described.naming));
                        if std::mem::take(&mut self.detect_described) {
                            self.detect = Some(app.detect_review(described.program.name));
                        }
                    }
                    Event::Described(None) => {}
                    event => match app.event(event) {
                        Some(Action::Quit) => return self.quit(app, rx),
                        Some(Action::Edit { path, line }) => self.edit(app, &path, line)?,
                        Some(Action::Compose(text)) => self.compose(app, &text)?,
                        Some(Action::Browse(url)) => browse(app, &url),
                        Some(Action::Review { id, request }) => self.request(id, &request),
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
                        Some(Action::EditLines { diff, picked, edit }) => {
                            self.edit_lines(diff, picked, edit);
                        }
                        Some(Action::PlanDiscard { diff, picked }) => {
                            self.plan_discard(diff, picked);
                        }
                        Some(Action::WriteResolution { path, choices }) => {
                            self.write_resolution(path, choices);
                        }
                        Some(Action::GitAdd(path)) => self.git_add(path),
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

    /// A frame when something changed, with the program status if that
    /// did, and the work the new frame wants.
    fn paint(&mut self, app: &mut App) -> io::Result<()> {
        // Before the frame: a diff in flight places what changed.
        if let Some(annotations) = app.take_annotations() {
            self.pool.annotate(annotations);
        }
        let Some(frame) = app.draw() else {
            return Ok(());
        };
        self.tty.write(frame)?;
        if let Some(report) = app.status_report() {
            self.tty.write(&report)?;
        }
        self.pool.replace(app.jobs());
        if let Some(gate) = &self.gate {
            gate.want(app.wanted());
        }
        Ok(())
    }

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

    /// Stages, unstages or discards `picked` lines of `diff` on a thread of
    /// their own, as `git apply` takes a process start; then the list is
    /// read again, index and all, which watch mode may not be on to do.
    fn edit_lines(&self, diff: Arc<FileDiff>, picked: Picked, edit: Edit) {
        let Some(repo) = self.files.as_ref().map(Repo::without_object_cache) else {
            self.events
                .send(Event::Edited(Err(NO_WORKTREE.to_owned())))
                .ok();
            return;
        };
        let (events, requests) = (self.events.clone(), self.requests.clone());
        let started = std::thread::Builder::new()
            .name("dffy-stage".into())
            .spawn(move || {
                let said = match repo.apply_lines(&diff, &picked, edit) {
                    Ok(done) if done.deleted => {
                        Ok(format!("deleted {}", escape(&diff.change.path)))
                    }
                    Ok(done) => {
                        let s = if done.lines == 1 { "" } else { "s" };
                        Ok(format!("{} {} line{s}", edit.done(), done.lines))
                    }
                    Err(e) => Err(e.to_string()),
                };
                if said.is_ok() {
                    requests.send(Request::Rescan).ok();
                }
                events.send(Event::Edited(said)).ok();
            });
        if let Err(e) = started {
            self.events
                .send(Event::Edited(Err(format!("couldn't {}: {e}", edit.verb()))))
                .ok();
        }
    }

    /// Works out what discarding `picked` lines of `diff` takes, on a
    /// thread of its own, as it reads the index and the file.
    fn plan_discard(&self, diff: Arc<FileDiff>, picked: Picked) {
        let Some(repo) = self.files.as_ref().map(Repo::without_object_cache) else {
            self.events
                .send(Event::Edited(Err(NO_WORKTREE.to_owned())))
                .ok();
            return;
        };
        let events = self.events.clone();
        let started = std::thread::Builder::new()
            .name("dffy-stage".into())
            .spawn(move || {
                let result = repo
                    .plan_lines(&diff, &picked, Edit::Discard)
                    .map_err(|e| e.to_string());
                events
                    .send(Event::Planned {
                        diff,
                        picked,
                        result,
                    })
                    .ok();
            });
        if let Err(e) = started {
            self.events
                .send(Event::Edited(Err(format!("couldn't discard: {e}"))))
                .ok();
        }
    }

    /// Writes what `choices` resolve the conflicted file `path` to, on a
    /// thread of its own; then the list is read again, as for
    /// [`Self::edit_lines`].
    fn write_resolution(&self, path: BString, choices: Arc<Choices>) {
        let Some(repo) = self.files.as_ref().map(Repo::without_object_cache) else {
            self.events
                .send(Event::Edited(Err(NO_WORKTREE.to_owned())))
                .ok();
            return;
        };
        let (events, requests) = (self.events.clone(), self.requests.clone());
        let thread = std::thread::Builder::new()
            .name("dffy-resolve".into())
            .spawn(move || {
                let result = repo
                    .write_resolution(path.as_bstr(), &choices)
                    .map_err(|e| e.to_string());
                if result.is_ok() {
                    requests.send(Request::Rescan).ok();
                }
                events.send(Event::Resolved { path, result }).ok();
            });
        if let Err(e) = thread {
            self.events
                .send(Event::Edited(Err(format!("couldn't write the file: {e}"))))
                .ok();
        }
    }

    /// `git add`s `path`, marking its conflict resolved, on a thread of its
    /// own; then the list is read again.
    fn git_add(&self, path: BString) {
        let Some(root) = self
            .files
            .as_ref()
            .and_then(|r| r.workdir())
            .map(Path::to_owned)
        else {
            self.events
                .send(Event::Edited(Err(NO_WORKTREE.to_owned())))
                .ok();
            return;
        };
        let (events, requests) = (self.events.clone(), self.requests.clone());
        let thread = std::thread::Builder::new()
            .name("dffy-add".into())
            .spawn(move || {
                let shown = escape(&path);
                let out = Command::new("git")
                    .current_dir(&root)
                    .args(["--literal-pathspecs", "add", "--"])
                    .arg(std::ffi::OsStr::from_bytes(&path))
                    .stdin(std::process::Stdio::null())
                    .output();
                let said = match out {
                    Ok(out) if out.status.success() => {
                        requests.send(Request::Rescan).ok();
                        Ok(format!("added {shown}: its conflict is resolved"))
                    }
                    Ok(out) => {
                        let said = String::from_utf8_lossy(&out.stderr);
                        let first = said.lines().next().unwrap_or("it failed").to_owned();
                        Err(format!("git add {shown}: {first}"))
                    }
                    Err(e) => Err(format!("couldn't run git: {e}")),
                };
                events.send(Event::Edited(said)).ok();
            });
        if let Err(e) = thread {
            self.events
                .send(Event::Edited(Err(format!("couldn't run git: {e}"))))
                .ok();
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
        self.in_editor(app, reader, &full, line)?;
        Ok(())
    }

    /// Returns whether the editor exited cleanly; why not goes in the
    /// status bar.
    fn in_editor(
        &self,
        app: &mut App,
        reader: &Reader,
        path: &Path,
        line: usize,
    ) -> io::Result<bool> {
        reader.pause();
        self.suspended.store(true, Ordering::Relaxed);
        term::restore();
        let status = self
            .tty
            .try_clone()
            .and_then(|tty| editor::open(path, line, tty));
        self.tty.enter()?;
        if !app.mouse() {
            self.tty.write(term::MOUSE_OFF)?;
        }
        self.suspended.store(false, Ordering::Relaxed);
        reader.resume();
        let (width, height) = self.tty.size();
        app.resize(width, height);
        app.invalidate();
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

    /// The review plugin's process, started in the working tree's root on
    /// first use, or why there's none. Its naming, once it has initialized,
    /// comes back as [`Event::Described`].
    fn plugin(&mut self) -> Result<&review::Plugin, String> {
        if self.plugin.is_none() {
            let name = self.review_name.clone().ok_or("no review plugin set")?;
            let full = format!("{}{name}", crate::plugin::REVIEW_PREFIX);
            let root = (self.files.as_ref().and_then(Repo::workdir))
                .ok_or("there's no working tree to run the review plugin in")?;
            let program = crate::plugin::find(crate::plugin::REVIEW_PREFIX, &name)
                .ok_or_else(|| format!("{full} isn't on PATH (see docs/plugins.md)"))?;
            let reviews =
                (self.files.as_ref()).and_then(|repo| crate::review::reviews_dir(repo, &name));
            let plugin = review::Plugin::start(&program, (Some(root), reviews), self.cache.clone())
                .map_err(|e| format!("couldn't start {full}: {e}"))?;
            let events = self.events.clone();
            plugin.on_ready(move |naming| {
                let described = naming
                    .ok()
                    .map(|naming| review::Described { program, naming });
                events.send(Event::Described(described)).ok();
            });
            let events = self.events.clone();
            plugin.on_push(move |push| events.send(Event::ReviewPush(push)).is_ok());
            self.plugin = Some(plugin);
        }
        self.plugin.as_ref().ok_or_else(String::new)
    }

    /// Asks the review plugin `request` as request `id`, what it says
    /// coming back as [`Event::Review`]. A `prepare`, `pull` or `post`
    /// cancels the one before it. A request that can't be made says why as
    /// its end.
    fn request(&mut self, id: u64, request: &review::Request) {
        if request.exclusive()
            && let Some(old) = self.review_run.take()
        {
            old.cancel();
        }
        let sink = self.events.clone();
        let run = match self.plugin() {
            Ok(plugin) => plugin.request(request, move |said| {
                sink.send(Event::Review { id, said }).is_ok()
            }),
            Err(why) => {
                sink.send(Event::Review {
                    id,
                    said: Said::Done(Err(why)),
                })
                .ok();
                return;
            }
        };
        if request.exclusive() {
            self.review_run = Some(run);
        }
    }

    /// The quit. One that stops a post running asks the plugin to stop it
    /// and waits for its end, as killing it mid-request could leave what it
    /// posted without its refs, posted again next time; what's being
    /// written is saved after.
    fn quit(&mut self, app: &mut App, rx: &mpsc::Receiver<Event>) -> io::Result<()> {
        if !app.posting_now() {
            return Ok(());
        }
        if let Some(run) = &self.review_run {
            run.cancel();
        }
        app.message("stopping the post…");
        self.paint(app)?;
        finish_post(app, rx);
        app.interrupted();
        Ok(())
    }

    /// Tells the review plugin which review the viewer shows, for it to
    /// watch.
    fn watch_review(&self, app: &App) {
        if let Some(plugin) = &self.plugin {
            plugin.watch(app.session_pointer().map(|(_, id, _)| id).as_deref());
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
            let from = (app.reviewed_from()).map(|(last, head)| (last.to_owned(), head.to_owned()));
            if let (Some((last, head)), Some(repo)) = (from, &self.files) {
                if !repo.is_revision(&last) {
                    if let Some(whole) = app.reviewed_head_missing() {
                        revisions = whole;
                    }
                } else if let Some(root) = repo.workdir() {
                    let (root, events) = (root.to_owned(), self.events.clone());
                    // A git process, which the loop spends drawing instead.
                    std::thread::Builder::new()
                        .name("dffy-review-rebased".into())
                        .spawn(move || {
                            if rewritten(&root, &last, &head) {
                                events.send(Event::ReviewRebased { last, head }).ok();
                            }
                        })
                        .ok();
                }
            }
            self.requests.send(Request::Retarget(revisions)).ok();
        }
        let repointed = comments.is_some() || moved.is_some();
        if let Some(path) = comments {
            let files = app.comment_files(path.clone());
            app.set_comments(path);
            self.watching = watch_comments(files, self.events.clone());
        }
        if repointed {
            self.point_session(app);
        }
        if let (Some((old, new)), Some(root)) = (moved, self.files.as_ref().and_then(Repo::workdir))
        {
            let (root, events) = (root.to_owned(), self.events.clone());
            // A git process takes milliseconds to start, which the loop
            // spends drawing instead.
            std::thread::Builder::new()
                .name("dffy-review-count".into())
                .spawn(move || {
                    let count = Command::new("git")
                        .args(["rev-list", "--count", &format!("{old}..{new}")])
                        .current_dir(root)
                        .stdin(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .output()
                        .ok()
                        .filter(|out| out.status.success())
                        .and_then(|out| String::from_utf8(out.stdout).ok()?.trim().parse().ok());
                    events.send(Event::ReviewMoved(count)).ok();
                })
                .ok();
        }
    }

    /// Writes a comment in the editor, from `text`, on a temporary file
    /// removed after; what's written goes to [`App::composed`]. Text that
    /// couldn't be saved, nor go back in the box, stays in a file of its
    /// own, which the status bar names; so does what an editor that failed
    /// wrote, as the box only gets back what it handed over.
    fn compose(&self, app: &mut App, text: &str) -> io::Result<()> {
        let Some(reader) = &self.reader else {
            app.composed(None);
            return Ok(());
        };
        let path = std::env::temp_dir().join(format!("dffy-comment-{}.md", std::process::id()));
        if let Err(e) = write_private(&path, text) {
            app.message(format!("couldn't write {}: {e}", path.display()));
            app.composed(None);
            return Ok(());
        }
        let clean = self.in_editor(app, reader, &path, 1)?;
        let written = std::fs::read_to_string(&path).ok();
        if !clean {
            app.composed(None);
            if written.is_some_and(|w| w != text) {
                let kept = keep(&path);
                app.message(format!(
                    "the editor failed; what it wrote is in {}",
                    kept.display()
                ));
                return Ok(());
            }
        } else if app.composed(written).is_some() {
            let kept = keep(&path);
            app.message(format!("not saved; the text is in {}", kept.display()));
            return Ok(());
        }
        std::fs::remove_file(&path).ok();
        Ok(())
    }

    /// Points the session pointer at the review session shown, for `dffy
    /// comments --session` and agents, and the review plugin at it, for it
    /// to watch. The pointer stays when the viewer exits; the next session
    /// replaces it.
    fn point_session(&self, app: &mut App) {
        self.watch_review(app);
        let (Some(repo), Some((comments, review, head))) = (&self.files, app.session_pointer())
        else {
            return;
        };
        let pointer = crate::review::Pointer {
            comments,
            review,
            branch: crate::review::current_branch(repo),
            head,
        };
        if let Err(e) = crate::review::write_session(repo, &pointer) {
            app.message(format!("the session pointer isn't written: {e}"));
        }
    }
}

/// Keeps `text`, a box's whose thread is gone, in a file of its own that
/// the status bar names.
fn keep_text(app: &mut App, text: &str) {
    let path = std::env::temp_dir().join(format!("dffy-comment-{}.md", std::process::id()));
    match write_private(&path, text) {
        Ok(()) => {
            let kept = keep(&path);
            app.message(format!(
                "the thread you were writing in is gone; your text is in {}",
                kept.display()
            ));
        }
        Err(e) => app.message(format!(
            "the thread you were writing in is gone, and its text couldn't be kept: {e}"
        )),
    }
}

/// Keeps the comment's file at `path` under a name of its own, which the
/// next comment's doesn't take; where it is now.
fn keep(path: &Path) -> PathBuf {
    let kept = path.with_file_name(format!(
        "dffy-comment-{}-{}.md",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis())
    ));
    match std::fs::rename(path, &kept) {
        Ok(()) => kept,
        Err(_) => path.to_owned(),
    }
}

/// Whether git in `root` says `last` isn't an ancestor of `head`, as after
/// a rebase; a git that can't tell says nothing.
fn rewritten(root: &Path, last: &str, head: &str) -> bool {
    Command::new("git")
        .args(["merge-base", "--is-ancestor", last, head])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.code() == Some(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many events a write makes is the backend's; the app's reload
    /// skips those that changed nothing.
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

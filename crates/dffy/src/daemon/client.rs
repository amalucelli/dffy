//! Watching a repository from a dffy view: through the broker when it's
//! reachable (starting it if needed), by itself otherwise. Either way the
//! caller sees the same events, and at most a note about watching alone.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use dffy_core::watch::{Notice, Root, Watcher};

use super::protocol::Message;

/// How long a view waits for a broker it started before watching alone.
const SPAWN_WAIT: Duration = Duration::from_millis(300);
/// How long the broker gets to answer a subscription. Setting up watches on
/// a kernel-size tree takes tens of milliseconds.
const HANDSHAKE: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    /// Watching from now on: changes before this may have gone unseen.
    /// Comes again after a switch to watching alone.
    Ready {
        backend: String,
        watches: u32,
    },
    Notice(Notice),
    /// The broker can't be used, for the reason given; this view watches by
    /// itself from the next [`Event::Ready`] on.
    Local(String),
}

type Sink = dyn Fn(Event) -> bool + Send + Sync;

/// A live watch; dropping it unsubscribes.
pub(crate) struct Subscription {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    stopped: AtomicBool,
    stream: Mutex<Option<UnixStream>>,
    local: Mutex<Option<Watcher>>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        if let Some(stream) = lock(&self.shared.stream).take() {
            stream.shutdown(std::net::Shutdown::Both).ok();
        }
        lock(&self.shared.local).take();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Watches `root`, handing events to `sink` on a thread of the
/// subscription's until it returns false. Returns at once; connecting, or
/// starting the broker, happens on that thread.
///
/// # Errors
///
/// When the thread can't start.
pub(crate) fn subscribe(
    root: Root,
    sink: impl Fn(Event) -> bool + Send + Sync + 'static,
) -> io::Result<Subscription> {
    let shared = Arc::new(Shared::default());
    let state = Arc::clone(&shared);
    let sink: Arc<Sink> = Arc::new(sink);
    thread::Builder::new()
        .name("dffy-subscription".into())
        .spawn(move || {
            let reason = match through_broker(&root, &state, &sink) {
                Ok(()) => "dffyd went away; watching in-process".to_owned(),
                Err(e) => format!("dffyd unavailable ({e}); watching in-process"),
            };
            if state.stopped.load(Ordering::Relaxed) || !sink(Event::Local(reason)) {
                return;
            }
            let notices = Arc::clone(&sink);
            let Ok(watcher) = Watcher::start(root, move |n| notices(Event::Notice(n))) else {
                return;
            };
            let ready = Event::Ready {
                backend: watcher.backend().to_owned(),
                watches: u32::try_from(watcher.watches()).unwrap_or(u32::MAX),
            };
            let mut local = lock(&state.local);
            if !state.stopped.load(Ordering::Relaxed) && sink(ready) {
                *local = Some(watcher);
            }
        })?;
    Ok(Subscription { shared })
}

/// Subscribes through the broker and relays its events until the
/// connection ends. `Ok` when it ended after the subscription was live.
fn through_broker(root: &Root, shared: &Shared, sink: &Arc<Sink>) -> io::Result<()> {
    let mut stream = connect_or_spawn(super::socket_path)?;
    stream.set_read_timeout(Some(HANDSHAKE))?;
    stream.write_all(&Message::Subscribe(root.worktree.clone()).encode())?;
    let ready = match Message::read(&mut stream)? {
        Message::Ready { backend, watches } => Event::Ready { backend, watches },
        Message::Error(e) => return Err(io::Error::other(e)),
        other => return Err(io::Error::other(format!("unexpected {other:?}"))),
    };
    stream.set_read_timeout(None)?;
    *lock(&shared.stream) = Some(stream.try_clone()?);
    if shared.stopped.load(Ordering::Relaxed) || !sink(ready) {
        return Ok(());
    }
    loop {
        let event = match Message::read(&mut stream) {
            Ok(Message::Changed(paths)) => Event::Notice(Notice::Changed(paths)),
            Ok(Message::Rescan) => Event::Notice(Notice::Rescan),
            Ok(Message::Polling(reason)) => Event::Notice(Notice::Polling(reason)),
            Ok(_) => continue,
            Err(_) => return Ok(()),
        };
        if !sink(event) {
            shared.stopped.store(true, Ordering::Relaxed);
            return Ok(());
        }
    }
}

/// Connects to dffyd's socket that `socket` names in the runtime
/// directory, the broker's or the control API's, starting dffyd when none
/// serves it.
pub(crate) fn connect_or_spawn(socket: fn(&Path) -> PathBuf) -> io::Result<UnixStream> {
    let path = socket(&super::runtime_dir()?);
    if let Ok(stream) = super::connect(&path) {
        return Ok(stream);
    }
    let mut child = Command::new(dffy()?)
        .arg(super::START)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir("/")
        .spawn()?;
    thread::Builder::new()
        .name("dffy-reap".into())
        .spawn(move || child.wait())?;
    let start = Instant::now();
    let mut wait = Duration::from_millis(2);
    loop {
        thread::sleep(wait);
        match super::connect(&path) {
            Ok(stream) => return Ok(stream),
            Err(e) if start.elapsed() >= SPAWN_WAIT => return Err(e),
            Err(_) => wait = (wait * 2).min(Duration::from_millis(40)),
        }
    }
}

/// `dffy __dffyd`: starts dffyd and exits, so dffyd is no descendant of
/// whoever wanted it, and outlives an agent ending its session by killing
/// all it started.
pub(crate) fn start() -> ExitCode {
    let started = dffy().and_then(|dffy| Command::new(dffy).args(["daemon", "run"]).spawn());
    match started {
        Ok(_) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// The program this process runs, where it is now.
fn dffy() -> io::Result<PathBuf> {
    std::env::current_exe().map(in_place)
}

/// `exe`, or what replaced it: Linux names the program of a process whose
/// file was replaced, as by an upgrade, with ` (deleted)` after it.
fn in_place(exe: PathBuf) -> PathBuf {
    let replaced = (exe.as_os_str().as_bytes().strip_suffix(b" (deleted)"))
        .map(|path| PathBuf::from(OsStr::from_bytes(path)))
        .filter(|path| path.exists());
    match replaced {
        Some(path) if !exe.exists() => path,
        _ => exe,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// After an upgrade, dffyd starts from the new program.
    #[test]
    fn a_replaced_program_is_run_from_its_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dffy");
        let deleted = dir.path().join("dffy (deleted)");
        assert_eq!(in_place(deleted.clone()), deleted, "nothing in its place");
        std::fs::write(&path, "").expect("write");
        assert_eq!(in_place(deleted.clone()), path);
        assert_eq!(in_place(path.clone()), path);
        std::fs::write(&deleted, "").expect("write");
        assert_eq!(in_place(deleted.clone()), deleted, "its own name");
    }
}

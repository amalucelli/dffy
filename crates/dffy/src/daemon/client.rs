//! Watching a repository from a dffy view: through the broker when it's
//! reachable (starting it if needed), by itself otherwise. Either way the
//! caller sees the same events, and at most a note about watching alone.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
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
    let mut stream = connect_or_spawn()?;
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

/// Connects to the broker, starting it when none runs.
fn connect_or_spawn() -> io::Result<UnixStream> {
    let dir = super::runtime_dir()?;
    if let Ok(stream) = super::connect(&dir) {
        return Ok(stream);
    }
    let mut child = Command::new(std::env::current_exe()?)
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir("/")
        .spawn()?;
    // Reaped here should it exit while this view runs; once this process
    // exits, init inherits it.
    thread::Builder::new()
        .name("dffy-reap".into())
        .spawn(move || child.wait())?;
    let start = Instant::now();
    let mut wait = Duration::from_millis(2);
    loop {
        thread::sleep(wait);
        match super::connect(&dir) {
            Ok(stream) => return Ok(stream),
            Err(e) if start.elapsed() >= SPAWN_WAIT => return Err(e),
            Err(_) => wait = (wait * 2).min(Duration::from_millis(40)),
        }
    }
}

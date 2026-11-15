//! The broker process, `dffy daemon run`: one [`Watcher`] per subscribed
//! repository, its notices fanned out to every subscriber, and an exit once
//! nobody has subscribed for a while.
//!
//! Each connection gets a thread reading it (to notice the hang-up) and,
//! for subscribers, a thread writing to it from an unbounded queue, so a
//! slow subscriber never holds up the others or the watcher. A subscriber
//! that can't take a write within a second is dropped; it falls back to
//! watching by itself.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::ops::Range;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use dffy_core::git::Repo;
use dffy_core::watch::{Notice, Watcher};
use rustix::fs::{FlockOperation, flock};

use super::protocol::{Message, Report, RootReport, VERSION};

/// How long the broker outlives its last subscriber, as gopls does.
const IDLE: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);
/// A broker exiting holds the lock until it's gone; a new one waits this
/// long for it before deciding another broker is running.
const LOCK_WAIT: Duration = Duration::from_millis(500);
/// How long a connection may stay silent before its request: a client
/// that never sends one would hold a thread for good.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// The wait after a failed accept, doubling to the end while they fail.
const ACCEPT_BACKOFF: Range<Duration> = Duration::from_millis(10)..Duration::from_secs(1);

struct Broker {
    started: Instant,
    socket: PathBuf,
    idle: Duration,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    roots: HashMap<PathBuf, Watched>,
    /// Since when nobody is subscribed; `None` while somebody is.
    idle_since: Option<Instant>,
    next_id: u64,
}

struct Watched {
    watcher: Watcher,
    subscribers: Vec<(u64, Sender<Arc<[u8]>>)>,
    polling: Option<String>,
}

/// Runs the broker until it's idle or told to stop. Returns quietly when
/// another broker already runs.
///
/// # Errors
///
/// When the runtime directory or socket can't be set up.
pub(crate) fn run() -> io::Result<()> {
    // Leave the terminal's session, so closing it doesn't take us along.
    let _ = rustix::process::setsid();
    let dir = super::runtime_dir()?;
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join(format!("broker-v{VERSION}.lock")))?;
    if !take_lock(&lock) {
        return Ok(());
    }
    // Whatever socket is there belongs to a broker that's gone, since the
    // lock is ours.
    let socket = super::socket_path(&dir);
    fs::remove_file(&socket).ok();
    let listener = UnixListener::bind(&socket)?;
    let idle = std::env::var("DFFY_DAEMON_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(IDLE, Duration::from_secs);
    let broker = Arc::new(Broker {
        started: Instant::now(),
        socket,
        idle,
        state: Mutex::new(State {
            idle_since: Some(Instant::now()),
            ..State::default()
        }),
    });
    let watchdog = Arc::clone(&broker);
    thread::Builder::new()
        .name("dffyd-idle".into())
        .spawn(move || watchdog.exit_when_idle())?;
    let mut backoff = ACCEPT_BACKOFF.start;
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            // Out of descriptors, most likely: retrying at once would spin
            // until a connection closes.
            thread::sleep(backoff);
            backoff = (backoff * 2).min(ACCEPT_BACKOFF.end);
            continue;
        };
        backoff = ACCEPT_BACKOFF.start;
        let broker = Arc::clone(&broker);
        // A connection that can't get a thread is dropped; its client
        // watches by itself.
        drop(
            thread::Builder::new()
                .name("dffyd-conn".into())
                .spawn(move || broker.serve(stream)),
        );
    }
    drop(lock);
    Ok(())
}

fn take_lock(lock: &File) -> bool {
    let start = Instant::now();
    loop {
        if flock(lock, FlockOperation::NonBlockingLockExclusive).is_ok() {
            return true;
        }
        if start.elapsed() > LOCK_WAIT {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

impl Broker {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn exit_when_idle(&self) {
        loop {
            thread::sleep(Duration::from_millis(200));
            let state = self.state();
            if state.idle_since.is_some_and(|at| at.elapsed() >= self.idle) {
                // Exiting under the lock: no subscription can slip in.
                self.exit();
            }
        }
    }

    fn exit(&self) -> ! {
        fs::remove_file(&self.socket).ok();
        std::process::exit(0);
    }

    fn serve(self: &Arc<Self>, mut stream: UnixStream) {
        if stream.set_read_timeout(Some(REQUEST_TIMEOUT)).is_err() {
            return;
        }
        let reply = match Message::read(&mut stream) {
            Ok(Message::Subscribe(path)) => return self.subscribe(stream, &path),
            Ok(Message::Status) => Message::Report(self.report()),
            Ok(Message::Describe) => Message::Report(Report {
                build: Some(crate::BUILD.to_owned()),
                ..self.report()
            }),
            Ok(Message::Stop) => {
                stream
                    .write_all(
                        &Message::Stopping {
                            pid: std::process::id(),
                        }
                        .encode(),
                    )
                    .ok();
                let _state = self.state();
                self.exit();
            }
            Ok(other) => Message::Error(format!("unexpected request {other:?}")),
            Err(_) => return,
        };
        stream.write_all(&reply.encode()).ok();
    }

    fn report(&self) -> Report {
        let state = self.state();
        let mut roots: Vec<RootReport> = state
            .roots
            .iter()
            .map(|(path, w)| RootReport {
                path: path.clone(),
                subscribers: u32::try_from(w.subscribers.len()).unwrap_or(u32::MAX),
                watches: u32::try_from(w.watcher.watches()).unwrap_or(u32::MAX),
                backend: w.watcher.backend().to_owned(),
                polling: w.polling.clone(),
            })
            .collect();
        roots.sort_by(|a, b| a.path.cmp(&b.path));
        Report {
            pid: std::process::id(),
            uptime_secs: self.started.elapsed().as_secs(),
            roots,
            build: None,
        }
    }

    fn subscribe(self: &Arc<Self>, mut stream: UnixStream, path: &Path) {
        let root = match Repo::discover(path).and_then(|r| r.watch_root()) {
            Ok(root) => root,
            Err(e) => {
                stream
                    .write_all(&Message::Error(e.to_string()).encode())
                    .ok();
                return;
            }
        };
        let key = root.worktree.clone();
        let (tx, rx) = mpsc::channel::<Arc<[u8]>>();
        // Watches are set up without the lock: on a large tree that takes
        // a while, and every other repository's notices wait on the lock.
        let mut started = None;
        let (id, greeting, spare) = loop {
            let mut state = self.state();
            // Nobody is subscribed while the watches are set up, but the
            // broker mustn't exit under this subscriber.
            state.idle_since = None;
            if !state.roots.contains_key(&key) {
                let Some(watcher) = started.take() else {
                    drop(state);
                    let broker = Arc::clone(self);
                    let fan_key = key.clone();
                    match Watcher::start(root.clone(), move |notice| {
                        broker.fan_out(&fan_key, &notice)
                    }) {
                        Ok(watcher) => started = Some(watcher),
                        Err(e) => {
                            let mut state = self.state();
                            if state.roots.is_empty() {
                                state.idle_since = Some(Instant::now());
                            }
                            drop(state);
                            stream
                                .write_all(&Message::Error(e.to_string()).encode())
                                .ok();
                            return;
                        }
                    }
                    continue;
                };
                let entry = Watched {
                    watcher,
                    subscribers: Vec::new(),
                    polling: None,
                };
                state.roots.insert(key.clone(), entry);
            }
            let id = state.next_id;
            state.next_id += 1;
            let Some(watched) = state.roots.get_mut(&key) else {
                return;
            };
            watched.subscribers.push((id, tx));
            let mut greeting = Message::Ready {
                backend: watched.watcher.backend().to_owned(),
                watches: u32::try_from(watched.watcher.watches()).unwrap_or(u32::MAX),
            }
            .encode();
            if let Some(reason) = &watched.polling {
                greeting.extend(Message::Polling(reason.clone()).encode());
            }
            // Another subscriber's watcher for the same tree came first.
            break (id, greeting, started.take());
        };
        // Outside the lock: a watcher stopping waits for its threads, which
        // take the lock to fan out.
        drop(spare);
        if let Ok(mut writer) = stream.try_clone()
            && writer.set_write_timeout(Some(WRITE_TIMEOUT)).is_ok()
        {
            drop(
                thread::Builder::new()
                    .name("dffyd-write".into())
                    .spawn(move || {
                        if writer.write_all(&greeting).is_ok() {
                            for frame in rx {
                                if writer.write_all(&frame).is_err() {
                                    break;
                                }
                            }
                        }
                        writer.shutdown(std::net::Shutdown::Both).ok();
                    }),
            );
        }
        // Subscribers send nothing more; this returns when they hang up or
        // the writer gave up on them, however long they stay.
        stream.set_read_timeout(None).ok();
        io::copy(&mut stream, &mut io::sink()).ok();
        self.unsubscribe(&key, id);
    }

    fn unsubscribe(&self, key: &Path, id: u64) {
        let mut state = self.state();
        let Some(watched) = state.roots.get_mut(key) else {
            return;
        };
        watched.subscribers.retain(|(i, _)| *i != id);
        if watched.subscribers.is_empty() {
            let gone = state.roots.remove(key);
            if state.roots.is_empty() {
                state.idle_since = Some(Instant::now());
            }
            drop(state);
            drop(gone);
        }
    }

    /// The watcher's sink: sends `notice` to every subscriber of `key`.
    fn fan_out(&self, key: &Path, notice: &Notice) -> bool {
        let message = match notice {
            Notice::Changed(paths) => Message::Changed(paths.clone()),
            Notice::Rescan => Message::Rescan,
            Notice::Polling(reason) => Message::Polling(reason.clone()),
        };
        let frame: Arc<[u8]> = message.encode().into();
        let mut state = self.state();
        let Some(watched) = state.roots.get_mut(key) else {
            return false;
        };
        if let Notice::Polling(reason) = notice {
            watched.polling = Some(reason.clone());
        }
        // A subscriber whose writer quit is removed by its reader thread,
        // which sees the hang-up.
        for (_, tx) in &watched.subscribers {
            tx.send(Arc::clone(&frame)).ok();
        }
        true
    }
}

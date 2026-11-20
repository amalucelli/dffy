//! Watching a repository for changes, so a diff can refresh in place.
//!
//! A backend (inotify on Linux, `FSEvents` on macOS) reports raw paths from a
//! thread that never waits on anyone: a stalled inotify reader overflows the
//! kernel queue. A second thread coalesces those paths into debounced
//! batches, keeps only paths a diff can depend on, and hands them to the
//! consumer. Paths git ignores and doesn't track are dropped as they arrive,
//! and on Linux their directories aren't watched at all. When the backend
//! can't watch (the inotify limit is per user and shared with every editor
//! and LSP), the second thread polls instead.

mod excludes;
#[cfg(target_os = "macos")]
mod fsevents;
#[cfg(target_os = "linux")]
mod inotify;

#[cfg(target_os = "macos")]
use fsevents as backend;
#[cfg(target_os = "linux")]
use inotify as backend;

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::io;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use gix::bstr::BString;

use crate::git::{DiffSpec, Repo};
use excludes::{Excludes, Tracked};

/// Quiet time that ends a batch. Editors save through several events (write
/// a temporary file, rename it over the original) within a few ms.
pub const DEBOUNCE: Duration = Duration::from_millis(8);
/// A batch goes out at most this long after its first event, so a long
/// stream of events (a checkout) still refreshes as it goes.
const MAX_DELAY: Duration = Duration::from_millis(100);
/// Larger batches go out as [`Notice::Rescan`]: re-listing everything is
/// cheaper than checking that many paths one by one.
pub const MAX_BATCH: usize = 4096;
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// What the watcher hands its sink, one batch at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// Absolute paths that changed, each covering everything under it when
    /// it was or is a directory.
    Changed(Vec<PathBuf>),
    /// Changes were lost or too many to list; anything may have changed.
    Rescan,
    /// The backend can't watch this root, for the reason given; it's polled
    /// once a second from now on.
    Polling(String),
}

/// A repository's directories, canonical. See [`Repo::watch_root`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Root {
    /// The working tree's top directory.
    pub worktree: PathBuf,
    /// Holds the index and `HEAD`; inside `common_dir` for a linked worktree.
    pub git_dir: PathBuf,
    /// Holds refs and `packed-refs`.
    pub common_dir: PathBuf,
    /// Set for a path compared outside git ([`Root::plain`]): then
    /// `worktree` is a plain directory, with no git directories watched and
    /// no exclude rules applied.
    pub plain: Option<Plain>,
}

/// What a [`Root`] outside git watches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Plain {
    /// Every directory below the root's `worktree`.
    Dir,
    /// Only this file in the root's `worktree`, not the directories below.
    File(std::ffi::OsString),
}

/// What a changed path means for a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A working-tree path, repository-relative; empty for the root.
    Worktree(BString),
    /// The index, `HEAD` or a ref: what the diff compares may have moved.
    Git,
}

impl Root {
    /// A file or directory outside git, to watch for a comparison of paths;
    /// its changes classify as [`Change::Worktree`] paths relative to it
    /// (the file's name, for a file). Symlinks are resolved.
    ///
    /// # Errors
    ///
    /// When `path` can't be resolved.
    pub fn plain(path: &Path) -> io::Result<Self> {
        let path = path.canonicalize()?;
        let (worktree, plain) = if path.is_dir() {
            (path, Plain::Dir)
        } else {
            match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => (parent.to_owned(), Plain::File(name.to_owned())),
                _ => (path, Plain::Dir),
            }
        };
        // Never watched or read: `plain` keeps every git-specific step off.
        let git_dir = worktree.join(".git");
        Ok(Self {
            common_dir: git_dir.clone(),
            git_dir,
            worktree,
            plain: Some(plain),
        })
    }

    /// Only the file `path`, which may not exist yet; its directory must.
    /// Watching the directory sees the file replaced by a rename, which
    /// atomic writers do.
    ///
    /// # Errors
    ///
    /// When `path`'s directory can't be resolved, or `path` has no name.
    pub fn file(path: &Path) -> io::Result<Self> {
        let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        let worktree = dir.canonicalize()?;
        // A placeholder: a plain root never watches or reads it.
        let git_dir = worktree.join(".git");
        Ok(Self {
            common_dir: git_dir.clone(),
            git_dir,
            worktree,
            plain: Some(Plain::File(name.to_owned())),
        })
    }

    /// What `path` means, `None` when no diff depends on it (git's object
    /// store, logs, lock files, nested repositories' git directories).
    #[must_use]
    pub fn classify(&self, path: &Path) -> Option<Change> {
        match &self.plain {
            Some(Plain::File(name)) => {
                let rel = path.strip_prefix(&self.worktree).ok()?;
                return (rel == Path::new(name))
                    .then(|| Change::Worktree(rel.as_os_str().as_bytes().into()));
            }
            Some(Plain::Dir) => {
                let rel = path.strip_prefix(&self.worktree).ok()?;
                return Some(Change::Worktree(rel.as_os_str().as_bytes().into()));
            }
            None => {}
        }
        let refs = |rel: &Path| {
            rel == Path::new("packed-refs")
                || (rel.starts_with("refs")
                    && rel.extension().is_none_or(|e| e != OsStr::new("lock")))
        };
        if let Ok(rel) = path.strip_prefix(&self.git_dir) {
            let state = rel == Path::new("index")
                || rel == Path::new("HEAD")
                || (self.git_dir == self.common_dir && refs(rel));
            return state.then_some(Change::Git);
        }
        if self.common_dir != self.git_dir
            && let Ok(rel) = path.strip_prefix(&self.common_dir)
        {
            return refs(rel).then_some(Change::Git);
        }
        let rel = path.strip_prefix(&self.worktree).ok()?;
        if rel
            .components()
            .any(|c| c == Component::Normal(OsStr::new(".git")))
        {
            return None;
        }
        Some(Change::Worktree(rel.as_os_str().as_bytes().into()))
    }
}

/// The inotify watches a [`Watcher`] on `root` would take now: one per
/// working-tree directory git doesn't ignore (or tracks files in), plus the
/// git directory, `info` and the refs. Walks the tree as a watcher starting
/// would, without watching anything.
#[cfg(target_os = "linux")]
#[must_use]
pub fn watches_needed(root: &Root) -> usize {
    let repo = Repo::discover(&root.worktree).ok();
    inotify::needed(root, &mut Excludes::load(root, repo.as_ref()))
}

/// What a backend sends the coalescing thread.
enum Raw {
    Path {
        path: PathBuf,
        is_dir: bool,
    },
    /// Events were lost; the backend has already rebuilt its watches.
    Overflow,
    /// The backend gave up, for the reason given, and released its watches.
    #[cfg_attr(
        target_os = "macos",
        expect(dead_code, reason = "FSEvents has no watch limit to run into")
    )]
    Exhausted(String),
    Stop,
}

#[derive(Default)]
struct Shared {
    watches: AtomicUsize,
    polling: AtomicBool,
}

/// The coalescer filters by these and reloads them; the inotify reader
/// decides from them which directories to watch.
type SharedExcludes = Arc<Mutex<Excludes>>;

fn lock(excludes: &Mutex<Excludes>) -> MutexGuard<'_, Excludes> {
    excludes.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running watch on one [`Root`]. Dropping it stops watching.
pub struct Watcher {
    tx: Sender<Raw>,
    shared: Arc<Shared>,
}

impl Watcher {
    /// Starts watching `root`. Notices go to `sink` on the watcher's thread,
    /// until it returns false or the watcher is dropped. Watches are in place
    /// when this returns, so later changes are never missed. A root that
    /// can't be watched is polled, starting with a [`Notice::Polling`].
    ///
    /// # Errors
    ///
    /// When the watcher's threads can't start.
    pub fn start(
        root: Root,
        sink: impl FnMut(Notice) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        // A plain directory inside some repository must not take its rules.
        let repo = root
            .plain
            .is_none()
            .then(|| Repo::discover(&root.worktree).ok())
            .flatten();
        let excludes = Arc::new(Mutex::new(Excludes::load(&root, repo.as_ref())));
        let backend = backend::start(&root, tx.clone(), &shared, &excludes);
        let state = Arc::clone(&shared);
        thread::Builder::new()
            .name("dffy-watch".into())
            .spawn(move || match backend {
                Ok(handle) => {
                    let watch = Watch {
                        root: &root,
                        handle,
                        excludes: &excludes,
                        repo: repo.as_ref(),
                    };
                    coalesce(watch, &rx, Box::new(sink), &state);
                }
                Err(reason) => poll(&root, &rx, reason, Box::new(sink), &state),
            })?;
        Ok(Self { tx, shared })
    }

    /// Directories watched (inotify) or paths in the stream (`FSEvents`).
    #[must_use]
    pub fn watches(&self) -> usize {
        self.shared.watches.load(Ordering::Relaxed)
    }

    /// `inotify`, `fsevents`, or `polling` once the backend gave up.
    #[must_use]
    pub fn backend(&self) -> &'static str {
        if self.shared.polling.load(Ordering::Relaxed) {
            "polling"
        } else {
            backend::NAME
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // Not joined: the thread may be inside the sink, which may be waiting
        // on whoever is dropping this.
        self.tx.send(Raw::Stop).ok();
    }
}

type Sink = Box<dyn FnMut(Notice) -> bool + Send>;

/// What the coalescing thread works with while the backend runs.
struct Watch<'a> {
    root: &'a Root,
    handle: backend::Handle,
    excludes: &'a Mutex<Excludes>,
    repo: Option<&'a Repo>,
}

impl Watch<'_> {
    /// Rereads the index; whether it could. Directories git now tracks
    /// files in need watching even when ignored.
    fn reload_tracked(&self) -> bool {
        // Read outside the lock: the inotify reader takes it too, and must
        // not stall behind a large index.
        let Some(tracked) = self.repo.and_then(Tracked::load) else {
            return false;
        };
        lock(self.excludes).set_tracked(tracked);
        true
    }
}

fn coalesce(watch: Watch<'_>, rx: &Receiver<Raw>, mut sink: Sink, shared: &Shared) {
    let root = watch.root;
    let index = root.git_dir.join("index");
    let mut pending = BTreeSet::new();
    let mut rules = Vec::new();
    let mut first: Option<Instant> = None;
    let mut last = Instant::now();
    loop {
        let raw = match first {
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(first) => {
                let deadline = (last + DEBOUNCE).min(first + MAX_DELAY);
                rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            }
        };
        let flush = match raw {
            Ok(Raw::Path { path, is_dir }) => {
                let mut excludes = lock(watch.excludes);
                let rule = excludes.is_rule_file(&path);
                let keep = match root.classify(&path) {
                    Some(Change::Worktree(_)) => !excludes.skip(&path, is_dir),
                    Some(Change::Git) => true,
                    None => false,
                };
                drop(excludes);
                if rule {
                    rules.push(path.clone());
                }
                if keep {
                    pending.insert(path);
                }
                if keep || rule {
                    last = Instant::now();
                    first.get_or_insert(last);
                }
                continue;
            }
            Ok(Raw::Overflow) => {
                pending.clear();
                rules.clear();
                first = None;
                // Changes to rule files or the index may be among the lost
                // events.
                lock(watch.excludes).reload_all();
                watch.reload_tracked();
                watch.handle.reevaluate(true);
                Notice::Rescan
            }
            Ok(Raw::Exhausted(reason)) => {
                drop(watch.handle);
                return poll(root, rx, reason, sink, shared);
            }
            Ok(Raw::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {
                first = None;
                let batch = std::mem::take(&mut pending);
                let rules_changed = lock(watch.excludes).reload(&std::mem::take(&mut rules));
                let tracked_changed = batch.contains(&index) && watch.reload_tracked();
                // Before the notice, so a consumer re-listing after it finds
                // the watches already in place.
                if rules_changed || tracked_changed {
                    watch.handle.reevaluate(rules_changed);
                }
                // What git lists as untracked may have changed anywhere.
                if rules_changed || batch.len() > MAX_BATCH {
                    Notice::Rescan
                } else {
                    Notice::Changed(batch.into_iter().collect())
                }
            }
        };
        if !sink(flush) {
            return;
        }
    }
}

/// When each file a poll looks at was last changed, as far as stat tells.
type Stamps = HashMap<PathBuf, Option<(i64, i64, u64, u64)>>;

/// Polls `root` once a second: the paths git reports as changed, plus the
/// index, `HEAD` and refs. Comparing stat data rather than the change list
/// catches a second edit to a file that was already modified.
// Costs one status scan per second (about 110ms on the kernel tree); per-root
// polling is the fallback for a full watch limit, not a steady state.
fn poll(root: &Root, rx: &Receiver<Raw>, reason: String, mut sink: Sink, shared: &Shared) {
    shared.polling.store(true, Ordering::Relaxed);
    shared.watches.store(0, Ordering::Relaxed);
    if !sink(Notice::Polling(reason)) {
        return;
    }
    let repo = match root.plain {
        Some(_) => None,
        None => match Repo::discover(&root.worktree) {
            Ok(repo) => Some(repo),
            Err(_) => return,
        },
    };
    let mut seen = stamps(repo.as_ref(), root);
    // Whatever changed before the first poll went unseen.
    if !sink(Notice::Rescan) {
        return;
    }
    loop {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(Raw::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = stamps(repo.as_ref(), root);
        let mut changed: Vec<PathBuf> = now
            .iter()
            .filter(|(path, stamp)| seen.get(*path) != Some(stamp))
            .chain(seen.iter().filter(|(path, _)| !now.contains_key(*path)))
            .map(|(path, _)| path.clone())
            .collect();
        seen = now;
        if changed.is_empty() {
            continue;
        }
        changed.sort_unstable();
        if !sink(Notice::Changed(changed)) {
            return;
        }
    }
}

/// Without `repo` (a [`Plain`] root), every file the root covers.
fn stamps(repo: Option<&Repo>, root: &Root) -> Stamps {
    let stamp = |path: &Path| {
        std::fs::symlink_metadata(path)
            .ok()
            .map(|m| (m.mtime(), m.mtime_nsec(), m.size(), m.ino()))
    };
    let mut stamps = Stamps::new();
    let Some(repo) = repo else {
        // Compared paths follow symlinks, so their targets are stamped.
        let stamp = |path: &Path| {
            std::fs::metadata(path)
                .ok()
                .map(|m| (m.mtime(), m.mtime_nsec(), m.size(), m.ino()))
        };
        let mut dirs = Vec::new();
        match &root.plain {
            Some(Plain::File(name)) => {
                let path = root.worktree.join(name);
                stamps.insert(path.clone(), stamp(&path));
            }
            _ => dirs.push(root.worktree.clone()),
        }
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    dirs.push(path);
                } else {
                    stamps.insert(path.clone(), stamp(&path));
                }
            }
        }
        return stamps;
    };
    // A failed scan (say, mid-checkout) shows up as changes next time.
    repo.for_each_change(&DiffSpec::IndexToWorktree { untracked: true }, &[], |c| {
        let path = root.worktree.join(OsStr::from_bytes(&c.path));
        stamps.insert(path.clone(), stamp(&path));
        ControlFlow::Continue(())
    })
    .ok();
    for path in [
        root.git_dir.join("index"),
        root.git_dir.join("HEAD"),
        root.common_dir.join("packed-refs"),
    ] {
        stamps.insert(path.clone(), stamp(&path));
    }
    let mut dirs = vec![root.common_dir.join("refs")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                dirs.push(path);
            } else {
                stamps.insert(path.clone(), stamp(&path));
            }
        }
    }
    stamps
}

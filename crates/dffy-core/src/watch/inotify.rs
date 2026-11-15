//! The Linux backend: one inotify watch per working-tree directory (inotify
//! isn't recursive), plus the git directory, refs and `info/exclude`. Git's
//! object store is never watched; it holds most of a repository's
//! directories and no diff depends on it directly. Neither are directories
//! git ignores and tracks nothing in, such as build output.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fs;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};

use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::io::Errno;
use rustix::pipe::{PipeFlags, pipe_with};

use super::excludes::Excludes;
use super::{Change, Plain, Raw, Root, Shared, SharedExcludes, lock};

pub(super) const NAME: &str = "inotify";

const MASK: WatchFlags = WatchFlags::CREATE
    .union(WatchFlags::DELETE)
    .union(WatchFlags::MODIFY)
    .union(WatchFlags::CLOSE_WRITE)
    .union(WatchFlags::ATTRIB)
    .union(WatchFlags::MOVED_FROM)
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::ONLYDIR)
    .union(WatchFlags::DONT_FOLLOW)
    .union(WatchFlags::EXCL_UNLINK);

enum Command {
    /// Re-decide which directories to watch: all of them when the rules
    /// changed, else only those the index decides. Done is dropped once the
    /// watches are in place.
    Reevaluate {
        rules_changed: bool,
        _done: Sender<()>,
    },
    Stop,
}

/// The reader thread; dropping it stops the thread and releases the watches.
pub(super) struct Handle {
    wake: OwnedFd,
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl Handle {
    /// Brings the watch set in line with the excludes, returning once it is.
    pub(super) fn reevaluate(&self, rules_changed: bool) {
        let (done, wait) = mpsc::channel();
        if self
            .commands
            .send(Command::Reevaluate {
                rules_changed,
                _done: done,
            })
            .is_ok()
        {
            let _ = rustix::io::write(&self.wake, b"r");
            // Disconnected once handled, or when the reader is gone.
            let _ = wait.recv();
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.commands.send(Command::Stop).ok();
        let _ = rustix::io::write(&self.wake, b"x");
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

/// Watches every directory of `root` that `excludes` doesn't skip before
/// returning; `Err` with a reason when inotify ran out of watches or
/// instances, having released whatever it had taken.
pub(super) fn start(
    root: &Root,
    tx: Sender<Raw>,
    shared: &Arc<Shared>,
    excludes: &SharedExcludes,
) -> Result<Handle, String> {
    let mut tree = Tree {
        root: root.clone(),
        fd: instance()?,
        dirs: HashMap::new(),
        skipped: BTreeSet::new(),
        excludes: Arc::clone(excludes),
    };
    tree.watch_all()?;
    shared.watches.store(tree.dirs.len(), Ordering::Relaxed);
    let shared = Arc::clone(shared);
    let (wake_rx, wake) = pipe_with(PipeFlags::CLOEXEC).map_err(|e| e.to_string())?;
    let (commands, commands_rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("dffy-inotify".into())
        .spawn(move || tree.run(&wake_rx, &commands_rx, &tx, &shared))
        .map_err(|e| e.to_string())?;
    Ok(Handle {
        wake,
        commands,
        thread: Some(thread),
    })
}

fn instance() -> Result<OwnedFd, String> {
    inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).map_err(|e| match e {
        Errno::MFILE => format!(
            "inotify instance limit reached (fs.inotify.max_user_instances = {})",
            limit("max_user_instances")
        ),
        e => format!("inotify unavailable: {e}"),
    })
}

fn limit(name: &str) -> String {
    fs::read_to_string(format!("/proc/sys/fs/inotify/{name}"))
        .map_or_else(|_| "?".to_owned(), |s| s.trim().to_owned())
}

struct Dir {
    path: PathBuf,
    /// Whether directories created inside get watched too.
    recursive: bool,
    /// Ignored by git, and watched only for the tracked files below it.
    ignored: bool,
}

/// Directory changes found while reading a batch, applied after it.
enum Action {
    /// A new directory, and whether its parent is ignored.
    Watch(PathBuf, bool),
    Forget(PathBuf),
}

struct Tree {
    root: Root,
    fd: OwnedFd,
    dirs: HashMap<i32, Dir>,
    /// The topmost working-tree directories left unwatched because git
    /// ignores them and tracks nothing inside.
    skipped: BTreeSet<PathBuf>,
    excludes: SharedExcludes,
}

/// How many watches [`start`] would take for `root` now, without taking
/// any: the same walk, counting instead of watching.
pub(super) fn needed(root: &Root, excludes: &mut Excludes) -> usize {
    let mut count = 0;
    let mut skipped = BTreeSet::new();
    // A directory that can't be watched (gone, unreadable) isn't counted,
    // as `add` skips it.
    walk_all(root, excludes, &mut skipped, &mut |dir, _, _| {
        if fs::metadata(dir).is_ok_and(|m| m.is_dir()) {
            count += 1;
        }
        Ok(())
    })
    .ok();
    count
}

/// Hands `add` every directory a watch on `root` covers, with whether it's
/// recursive and ignored: the working tree's (see [`walk`]), the git
/// directories, `info` and the refs. A plain root has no git directories,
/// and a plain file's root only its own directory.
fn walk_all(
    root: &Root,
    excludes: &mut Excludes,
    skipped: &mut BTreeSet<PathBuf>,
    add: &mut dyn FnMut(&Path, bool, bool) -> Result<(), String>,
) -> Result<(), String> {
    match &root.plain {
        Some(Plain::File(_)) => return add(&root.worktree, false, false),
        Some(Plain::Dir) => return walk(&root.worktree, false, excludes, skipped, add),
        None => {}
    }
    walk(&root.worktree, false, excludes, skipped, add)?;
    add(&root.git_dir, false, false)?;
    if root.common_dir != root.git_dir {
        add(&root.common_dir, false, false)?;
    }
    add(&root.common_dir.join("info"), false, false)?;
    walk(&root.common_dir.join("refs"), false, excludes, skipped, add)
}

/// Hands `add` `top` and every directory below it, except `.git` ones and
/// those git ignores and tracks nothing in, which go to `skipped` and are
/// left out with everything below them. Each directory goes to `add`
/// before it's listed, so a directory created meanwhile is either listed or
/// reported by its new watch.
fn walk(
    top: &Path,
    parent_ignored: bool,
    excludes: &mut Excludes,
    skipped: &mut BTreeSet<PathBuf>,
    add: &mut dyn FnMut(&Path, bool, bool) -> Result<(), String>,
) -> Result<(), String> {
    let mut stack = vec![(top.to_owned(), parent_ignored)];
    while let Some((dir, parent_ignored)) = stack.pop() {
        let ignored = excludes.child_dir_ignored(&dir, parent_ignored);
        if ignored && !excludes.tracked(&dir) {
            skipped.insert(dir);
            continue;
        }
        add(&dir, true, ignored)?;
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) && entry.file_name() != ".git" {
                stack.push((entry.path(), ignored));
            }
        }
    }
    Ok(())
}

fn add_watch(
    fd: &OwnedFd,
    dirs: &mut HashMap<i32, Dir>,
    dir: &Path,
    recursive: bool,
    ignored: bool,
) -> Result<(), String> {
    match inotify::add_watch(fd, dir, MASK) {
        Ok(wd) => {
            dirs.insert(
                wd,
                Dir {
                    path: dir.to_owned(),
                    recursive,
                    ignored,
                },
            );
            Ok(())
        }
        Err(Errno::NOSPC) => Err(format!(
            "inotify watch limit reached (fs.inotify.max_user_watches = {})",
            limit("max_user_watches")
        )),
        // Gone already, or unreadable: nothing to watch there.
        Err(_) => Ok(()),
    }
}

impl Tree {
    fn watch_all(&mut self) -> Result<(), String> {
        let excludes = Arc::clone(&self.excludes);
        let (fd, dirs) = (&self.fd, &mut self.dirs);
        walk_all(
            &self.root,
            &mut lock(&excludes),
            &mut self.skipped,
            &mut |dir, recursive, ignored| add_watch(fd, dirs, dir, recursive, ignored),
        )
    }

    /// Watches `top` and the directories below it that need it; see
    /// [`walk`].
    fn add_tree(&mut self, top: &Path, parent_ignored: bool) -> Result<(), String> {
        let excludes = Arc::clone(&self.excludes);
        let (fd, dirs) = (&self.fd, &mut self.dirs);
        walk(
            top,
            parent_ignored,
            &mut lock(&excludes),
            &mut self.skipped,
            &mut |dir, recursive, ignored| add_watch(fd, dirs, dir, recursive, ignored),
        )
    }

    /// Drops the watches on `top` and below: it moved, and its watches would
    /// keep reporting the old paths.
    fn forget(&mut self, top: &Path) {
        let fd = &self.fd;
        self.dirs.retain(|&wd, dir| {
            let keep = !dir.path.starts_with(top);
            if !keep {
                let _ = inotify::remove_watch(fd, wd);
            }
            keep
        });
        self.skipped.retain(|dir| !dir.starts_with(top));
    }

    /// Unwatches working-tree directories git now ignores and tracks nothing
    /// in, then watches whatever the skipped ones no longer hide.
    fn reevaluate(&mut self, rules_changed: bool) -> Result<(), String> {
        let excludes = Arc::clone(&self.excludes);
        let mut ex = lock(&excludes);
        // Unless the rules changed, only the tracked files can decide
        // differently, and only for ignored directories.
        let mut watched: Vec<(i32, PathBuf)> = self
            .dirs
            .iter()
            .filter(|(_, dir)| {
                (rules_changed || dir.ignored)
                    && dir.recursive
                    && dir.path != self.root.worktree
                    && matches!(self.root.classify(&dir.path), Some(Change::Worktree(_)))
            })
            .map(|(&wd, dir)| (wd, dir.path.clone()))
            .collect();
        // Parents first, each directory's subtree right after it.
        watched.sort_unstable_by(|a, b| a.1.cmp(&b.1));
        let mut unwatch: Vec<PathBuf> = Vec::new();
        for (wd, path) in watched {
            if unwatch.last().is_some_and(|top| path.starts_with(top)) {
                continue;
            }
            let parent_ignored = path.parent().is_some_and(|p| ex.dir_ignored(p));
            let ignored = ex.child_dir_ignored(&path, parent_ignored);
            if ignored && !ex.tracked(&path) {
                unwatch.push(path);
            } else if let Some(dir) = self.dirs.get_mut(&wd) {
                dir.ignored = ignored;
            }
        }
        let mut uncover = Vec::new();
        for dir in std::mem::take(&mut self.skipped) {
            let parent_ignored = dir.parent().is_some_and(|p| ex.dir_ignored(p));
            if unwatch.iter().any(|top| dir.starts_with(top)) {
                continue;
            }
            if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            uncover.push((dir, parent_ignored));
        }
        drop(ex);
        for top in unwatch {
            self.forget(&top);
            self.skipped.insert(top);
        }
        for (dir, parent_ignored) in uncover {
            self.add_tree(&dir, parent_ignored)?;
        }
        Ok(())
    }

    fn run(
        mut self,
        wake: &OwnedFd,
        commands: &Receiver<Command>,
        tx: &Sender<Raw>,
        shared: &Shared,
    ) {
        let mut buf = vec![MaybeUninit::<u8>::uninit(); 64 << 10];
        loop {
            let mut fds = [
                PollFd::new(&self.fd, PollFlags::IN),
                PollFd::new(wake, PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(_) => return,
            }
            let woken = !fds[1].revents().is_empty();
            let mut result = self.read(&mut buf, tx);
            if woken && result.is_ok() {
                let _ = rustix::io::read(wake, &mut [0; 64]);
                loop {
                    match commands.try_recv() {
                        Ok(Command::Reevaluate {
                            rules_changed,
                            _done,
                        }) => {
                            result = self.reevaluate(rules_changed);
                            if result.is_err() {
                                break;
                            }
                            // Counted before `_done` goes, so the count is
                            // current once the caller hears back.
                            shared.watches.store(self.dirs.len(), Ordering::Relaxed);
                        }
                        Ok(Command::Stop) | Err(TryRecvError::Disconnected) => return,
                        Err(TryRecvError::Empty) => break,
                    }
                }
            }
            if let Err(reason) = result {
                // Dropping `self` closes the instance, releasing its watches.
                tx.send(Raw::Exhausted(reason)).ok();
                return;
            }
            shared.watches.store(self.dirs.len(), Ordering::Relaxed);
        }
    }

    /// Reads every queued event, reporting paths as it goes.
    fn read(&mut self, buf: &mut [MaybeUninit<u8>], tx: &Sender<Raw>) -> Result<(), String> {
        let mut actions = Vec::new();
        let mut overflow = false;
        let mut reader = inotify::Reader::new(&self.fd, buf);
        loop {
            let event = match reader.next() {
                Ok(event) => event,
                Err(Errno::INTR) => continue,
                // Drained (AGAIN), or the instance is broken.
                Err(_) => break,
            };
            let flags = event.events();
            if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                overflow = true;
                continue;
            }
            if flags.contains(ReadFlags::IGNORED) {
                self.dirs.remove(&event.wd());
                continue;
            }
            // Events about a watched directory itself carry no name; its
            // parent reports the same change by name.
            let (Some(dir), Some(name)) = (self.dirs.get(&event.wd()), event.file_name()) else {
                continue;
            };
            let name = OsStr::from_bytes(name.to_bytes());
            let path = dir.path.join(name);
            let is_dir = flags.contains(ReadFlags::ISDIR);
            if is_dir && dir.recursive && name != ".git" {
                if flags.intersects(ReadFlags::CREATE | ReadFlags::MOVED_TO) {
                    actions.push(Action::Watch(path.clone(), dir.ignored));
                } else if flags.contains(ReadFlags::MOVED_FROM) {
                    actions.push(Action::Forget(path.clone()));
                }
            }
            tx.send(Raw::Path { path, is_dir }).ok();
        }
        if overflow {
            // The watch set may have missed directories created during the
            // overflow; rebuilding is a fresh walk, ~10ms on the kernel tree.
            self.fd = instance()?;
            self.dirs.clear();
            self.skipped.clear();
            self.watch_all()?;
            tx.send(Raw::Overflow).ok();
            return Ok(());
        }
        for action in actions {
            match action {
                // The new directory's path covers whatever was created in it
                // before its watch landed.
                Action::Watch(path, parent_ignored) => self.add_tree(&path, parent_ignored)?,
                Action::Forget(path) => self.forget(&path),
            }
        }
        Ok(())
    }
}

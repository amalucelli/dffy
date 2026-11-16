//! Two files or directories compared outside git, as `git diff --no-index`
//! and `diff -r` compare them. The pair stands in for a repository behind
//! [`Repo`](super::Repo): it lists changes in git's path order, streaming
//! them as the walk finds them, and loads their sides from disk each time,
//! so the rest of dffy (renames, diffs, the viewer, watching) works on it
//! unchanged.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::num::NonZero;
use std::ops::ControlFlow;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread;

use gix::bstr::{BStr, BString, ByteSlice};
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use super::{Error, FileChange, Filter, Mode, PathRoot, Result, Side};
use crate::watch::Root;

/// One side of a comparison outside git.
#[derive(Debug, Clone)]
pub enum Endpoint {
    /// A file or directory on disk; symlinks are followed. A pipe, such as
    /// `<(command)`, is read whole up front.
    Path(PathBuf),
    /// A file's whole content, read up front: stdin's, for `-`.
    Bytes(Vec<u8>),
    /// No file at all (`/dev/null`), so the other side reads as added or
    /// deleted.
    Missing,
}

/// What [`Repo::compare`](super::Repo::compare) compares, and how.
///
/// Two directories are walked in step and their files paired by relative
/// path, giving one change per file that differs (added, deleted, modified
/// or a type change), in git's path order. Nothing is left out by default,
/// as with `diff -r`: not dot files, `.git`, or what `.gitignore` files
/// list. Symlinks are followed, as `diff -r` does, and as `git difftool
/// --dir-diff` needs: its working-tree side is a tree of links. A link
/// whose target is missing reads as the target's path, the way git stores
/// a link. FIFOs, sockets and devices are left out.
///
/// Two files, stdin or `/dev/null` give at most one change, at
/// [`Compare::name`]. A file against a directory is compared with the file
/// of the same name inside it, as `diff` does.
#[derive(Debug, Clone)]
pub struct Compare {
    /// The first side.
    pub old: Endpoint,
    /// The second side.
    pub new: Endpoint,
    /// The change's path when two files are compared: what the viewer shows,
    /// and what the language is detected from. Unused for directories.
    pub name: BString,
    /// Gitignore-style patterns for what a directory walk leaves out, matched
    /// against paths relative to the compared directories: `*.o` and
    /// `target/` anywhere, `src/gen` from the top only.
    pub exclude: Vec<String>,
    /// Also leave out what each directory's `.gitignore` files list, and
    /// `.git` directories, as git's untracked walk would. Each side's files
    /// follow that side's rules.
    pub gitignore: bool,
}

/// [`Compare`] with both sides resolved to what's on disk.
pub(super) struct Pair {
    old: End,
    new: End,
    name: BString,
    exclude: Gitignore,
    gitignore: bool,
}

enum End {
    Dir(PathBuf),
    File(PathBuf),
    Bytes(Vec<u8>),
    Missing,
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

impl Pair {
    pub(super) fn new(compare: Compare) -> Result<Self> {
        let end = |endpoint| -> Result<End> {
            Ok(match endpoint {
                Endpoint::Path(path) => {
                    let meta = fs::metadata(&path).map_err(io_error(&path))?;
                    let kind = meta.file_type();
                    if kind.is_dir() {
                        End::Dir(path)
                    } else if kind.is_file() {
                        End::File(path)
                    } else if kind.is_fifo() {
                        // A pipe, as from `<(command)`, reads once and
                        // stats as empty: read it whole now, as stdin.
                        End::Bytes(fs::read(&path).map_err(io_error(&path))?)
                    } else {
                        return Err(Error::Uncomparable(format!(
                            "'{}' is neither a file nor a directory",
                            path.display()
                        )));
                    }
                }
                Endpoint::Bytes(bytes) => End::Bytes(bytes),
                Endpoint::Missing => End::Missing,
            })
        };
        let (mut old, mut new) = (end(compare.old)?, end(compare.new)?);
        match (&old, &new) {
            (End::Dir(dir), End::File(file)) => old = same_name(dir, file)?,
            (End::File(file), End::Dir(dir)) => new = same_name(dir, file)?,
            (End::Dir(_), End::Bytes(_)) | (End::Bytes(_), End::Dir(_)) => {
                return Err(Error::Uncomparable(
                    "stdin can't be compared with a directory".to_owned(),
                ));
            }
            (End::Dir(_), End::Missing) | (End::Missing, End::Dir(_)) => {
                return Err(Error::Uncomparable(
                    "/dev/null can't be compared with a directory".to_owned(),
                ));
            }
            _ => {}
        }
        let mut builder = GitignoreBuilder::new("");
        for pattern in &compare.exclude {
            builder
                .add_line(None, pattern)
                .map_err(|e| Error::Uncomparable(format!("bad --exclude pattern: {e}")))?;
        }
        let exclude = builder
            .build()
            .map_err(|e| Error::Uncomparable(format!("bad --exclude pattern: {e}")))?;
        Ok(Self {
            old,
            new,
            name: compare.name,
            exclude,
            gitignore: compare.gitignore,
        })
    }

    fn end(&self, root: PathRoot) -> &End {
        match root {
            PathRoot::Old => &self.old,
            PathRoot::New => &self.new,
        }
    }

    /// Hands each change to `emit`, in path order, as it's found.
    pub(super) fn changes(
        &self,
        filter: &Filter,
        emit: &mut dyn FnMut(FileChange) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>> {
        if let (End::Dir(old), End::Dir(new)) = (&self.old, &self.new) {
            return self.walk(old, new, filter, emit);
        }
        if !filter.matches(&self.name) {
            return Ok(ControlFlow::Continue(()));
        }
        let side = |root| -> Result<Option<(Side, Content<'_>)>> {
            Ok(match self.end(root) {
                End::File(path) => {
                    let meta = fs::metadata(path).map_err(io_error(path))?;
                    Some((
                        Side::Path {
                            mode: mode(&meta),
                            root,
                        },
                        Content::File(path, meta),
                    ))
                }
                End::Bytes(bytes) => Some((
                    Side::Path {
                        mode: Mode::File,
                        root,
                    },
                    Content::Bytes(bytes),
                )),
                End::Missing | End::Dir(_) => None,
            })
        };
        let (old, new) = (side(PathRoot::Old)?, side(PathRoot::New)?);
        let same = match (&old, &new) {
            (Some((o, oc)), Some((n, nc))) => o.mode() == n.mode() && oc.same(nc)?,
            (None, None) => true,
            _ => false,
        };
        if same {
            return Ok(ControlFlow::Continue(()));
        }
        Ok(emit(FileChange::new(
            self.name.clone(),
            old.map(|s| s.0),
            new.map(|s| s.0),
        )))
    }

    /// Walks the two directories on one thread while the others look at
    /// each file found: its size and mode on both sides, and its bytes when
    /// the sizes match, which on two copies of a tree is nearly every file.
    /// Changes reach `emit` in walk order, each as soon as everything
    /// before it is decided.
    fn walk(
        &self,
        old: &Path,
        new: &Path,
        filter: &Filter,
        emit: &mut dyn FnMut(FileChange) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>> {
        // The walk mostly waits on the directory reads, so a worker per core.
        let workers = thread::available_parallelism().map_or(1, NonZero::get);
        let stop = AtomicBool::new(false);
        let (jobs, queue) = mpsc::channel::<Vec<(usize, Found)>>();
        let queue = Mutex::new(queue);
        let (done, results) = mpsc::channel::<Decided>();
        thread::scope(|scope| {
            for _ in 0..workers {
                let (queue, done, stop) = (&queue, done.clone(), &stop);
                scope.spawn(move || {
                    loop {
                        let job = queue.lock().unwrap_or_else(PoisonError::into_inner).recv();
                        let Ok(batch) = job else { break };
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let decided = batch
                            .into_iter()
                            .map(|(seq, found)| (seq, found.decide()))
                            .collect();
                        if done.send(Decided::Items(decided)).is_err() {
                            break;
                        }
                    }
                });
            }
            let (walked, stopped) = (done.clone(), &stop);
            scope.spawn(move || {
                let mut seq = 0;
                let mut batch = Vec::with_capacity(BATCH);
                // The sink borrows `seq` and `batch` until the walk is done.
                let result = {
                    let mut sink = |found| {
                        batch.push((seq, found));
                        seq += 1;
                        // Small batches first, so the first change isn't held
                        // until a full batch is listed.
                        let sent = batch.len() < (seq / 8).clamp(1, BATCH)
                            || jobs
                                .send(std::mem::replace(&mut batch, Vec::with_capacity(BATCH)))
                                .is_ok();
                        if sent && !stopped.load(Ordering::Relaxed) {
                            ControlFlow::Continue(())
                        } else {
                            ControlFlow::Break(())
                        }
                    };
                    let mut walk = Walk {
                        pair: self,
                        filter,
                        sink: &mut sink,
                        ancestors: [Vec::new(), Vec::new()],
                        rules: [Vec::new(), Vec::new()],
                    };
                    walk.run([Some(old), Some(new)])
                };
                if !batch.is_empty() {
                    jobs.send(batch).ok();
                }
                // Workers stop once the queue is empty and closed.
                drop(jobs);
                walked.send(Decided::End(seq, result)).ok();
            });
            drop(done);
            let result = in_order(&results, emit);
            stop.store(true, Ordering::Relaxed);
            result
        })
    }

    /// Reads one side of the file at `path` (relative to the compared
    /// directories; ignored for files).
    pub(super) fn load(&self, path: &BStr, root: PathRoot, mode: Mode) -> Result<Vec<u8>> {
        let full = match self.end(root) {
            End::Bytes(bytes) => return Ok(bytes.clone()),
            End::Missing => return Ok(Vec::new()),
            End::File(file) => file.clone(),
            End::Dir(dir) => dir.join(OsStr::from_bytes(path)),
        };
        if mode == Mode::Symlink {
            return Ok(fs::read_link(&full)
                .map_err(io_error(&full))?
                .into_os_string()
                .into_vec());
        }
        fs::read(&full).map_err(io_error(&full))
    }

    /// The size of one side of the file at `path`, as [`Pair::load`] would
    /// read it; links whose target is missing count as empty.
    pub(super) fn size(&self, path: &BStr, root: PathRoot, mode: Mode) -> Result<u64> {
        let full = match self.end(root) {
            End::Bytes(bytes) => return Ok(bytes.len() as u64),
            End::Missing => return Ok(0),
            _ if mode == Mode::Symlink => return Ok(0),
            End::File(file) => file.clone(),
            End::Dir(dir) => dir.join(OsStr::from_bytes(path)),
        };
        fs::metadata(&full)
            .map(|m| m.len())
            .map_err(io_error(&full))
    }

    /// The file behind `path` on the second side, or on the first when the
    /// second has none on disk.
    pub(super) fn on_disk(&self, path: &BStr) -> Option<PathBuf> {
        [&self.new, &self.old]
            .into_iter()
            .find_map(|end| match end {
                End::File(file) => Some(file.clone()),
                End::Dir(dir) => {
                    let full = dir.join(OsStr::from_bytes(path));
                    full.exists().then_some(full)
                }
                End::Bytes(_) | End::Missing => None,
            })
    }

    pub(super) fn watch_roots(&self) -> Vec<Root> {
        [&self.old, &self.new]
            .into_iter()
            .filter_map(|end| match end {
                End::File(path) | End::Dir(path) => Root::plain(path).ok(),
                End::Bytes(_) | End::Missing => None,
            })
            .collect()
    }
}

/// One compared file's content, for telling whether the two are the same.
enum Content<'a> {
    Bytes(&'a [u8]),
    File(&'a Path, Metadata),
}

impl Content<'_> {
    fn same(&self, other: &Self) -> Result<bool> {
        match (self, other) {
            (Self::Bytes(a), Self::Bytes(b)) => Ok(a == b),
            (Self::Bytes(bytes), Self::File(path, meta))
            | (Self::File(path, meta), Self::Bytes(bytes)) => Ok(meta.len() == bytes.len() as u64
                && fs::read(path).map_err(io_error(path))? == *bytes),
            (Self::File(a, a_meta), Self::File(b, b_meta)) => {
                same_content(a, a_meta, b, b_meta).map_err(io_error(b))
            }
        }
    }
}

/// `file`'s counterpart in `dir`: the file there with the same name.
fn same_name(dir: &Path, file: &Path) -> Result<End> {
    let Some(name) = file.file_name() else {
        return Err(Error::Uncomparable(format!(
            "'{}' has no file name to look for in '{}'",
            file.display(),
            dir.display()
        )));
    };
    let path = dir.join(name);
    let meta = fs::metadata(&path).map_err(io_error(&path))?;
    if meta.is_dir() {
        return Err(Error::Uncomparable(format!(
            "'{}' is a directory, and '{}' a file",
            path.display(),
            file.display()
        )));
    }
    // Reading a FIFO or a device found here could block for good, so only
    // a regular file is compared, as in a directory walk.
    if !meta.is_file() {
        return Err(Error::Uncomparable(format!(
            "'{}' is neither a file nor a directory",
            path.display()
        )));
    }
    Ok(End::File(path))
}

fn mode(meta: &Metadata) -> Mode {
    if meta.permissions().mode() & 0o111 == 0 {
        Mode::File
    } else {
        Mode::Executable
    }
}

/// A file the walk found on one side or both, with where and what it is on
/// each.
struct Found {
    rel: BString,
    sides: [Option<(PathBuf, Kind)>; 2],
}

impl Found {
    /// The change, if the file differs between the sides. A file deleted
    /// since it was listed (an editor replacing it, say) reads as missing
    /// on that side; the next refresh settles it.
    fn decide(self) -> Result<Option<FileChange>> {
        let [old, new] = self.sides;
        let side =
            |entry: &Option<(PathBuf, Kind)>, root| -> Result<Option<(Side, Option<Metadata>)>> {
                let Some((path, kind)) = entry else {
                    return Ok(None);
                };
                if *kind == Kind::Dangling {
                    return Ok(Some((
                        Side::Path {
                            mode: Mode::Symlink,
                            root,
                        },
                        None,
                    )));
                }
                match fs::metadata(path) {
                    Ok(meta) => Ok(Some((
                        Side::Path {
                            mode: mode(&meta),
                            root,
                        },
                        Some(meta),
                    ))),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(io_error(path)(e)),
                }
            };
        let (o, n) = (side(&old, PathRoot::Old)?, side(&new, PathRoot::New)?);
        if let (Some((os, om)), Some((ns, nm)), Some((op, _)), Some((np, _))) = (&o, &n, &old, &new)
            && os.mode() == ns.mode()
        {
            let same = match (om, nm) {
                // A side that can't be read may differ: it's listed, and
                // reading it for the diff names the side that failed,
                // while the walk goes on.
                (Some(om), Some(nm)) => same_content(op, om, np, nm).unwrap_or(false),
                _ => fs::read_link(op).ok() == fs::read_link(np).ok(),
            };
            if same {
                return Ok(None);
            }
        }
        if o.is_none() && n.is_none() {
            return Ok(None);
        }
        Ok(Some(FileChange::new(
            self.rel,
            o.map(|s| s.0),
            n.map(|s| s.0),
        )))
    }
}

/// Files handed to a worker at a time: one at a time, the hand-offs cost
/// more than comparing two copies of a small file.
const BATCH: usize = 32;

/// What became of some of the walk's finds, by their place in walk order,
/// or the walk's end with how many it found.
enum Decided {
    Items(Vec<(usize, Result<Option<FileChange>>)>),
    End(usize, Result<ControlFlow<()>>),
}

/// Hands the changes arriving on `results` to `emit` in walk order.
fn in_order(
    results: &mpsc::Receiver<Decided>,
    emit: &mut dyn FnMut(FileChange) -> ControlFlow<()>,
) -> Result<ControlFlow<()>> {
    let mut pending = BTreeMap::new();
    let mut next = 0;
    let mut total = None;
    while total.is_none_or(|total| next < total) {
        let Ok(decided) = results.recv() else {
            return Err(Error::Uncomparable(
                "a thread comparing files stopped unexpectedly".to_owned(),
            ));
        };
        match decided {
            Decided::Items(items) => pending.extend(items),
            Decided::End(found, walked) => {
                // A walk only breaks off once this loop has stopped.
                let _: ControlFlow<()> = walked?;
                total = Some(found);
            }
        }
        while let Some(result) = pending.remove(&next) {
            next += 1;
            if let Some(change) = result?
                && emit(change).is_break()
            {
                return Ok(ControlFlow::Break(()));
            }
        }
    }
    Ok(ControlFlow::Continue(()))
}

/// Files whose sizes match are read in chunks this big and compared.
const CHUNK: usize = 64 << 10;

fn same_content(a: &Path, a_meta: &Metadata, b: &Path, b_meta: &Metadata) -> io::Result<bool> {
    if a_meta.len() != b_meta.len() {
        return Ok(false);
    }
    if a_meta.dev() == b_meta.dev() && a_meta.ino() == b_meta.ino() {
        return Ok(true);
    }
    same_bytes(a, b, a_meta.len())
}

thread_local! {
    /// Kept between files: zeroing fresh buffers for each of thousands of
    /// small files cost more than reading them.
    static BUFFERS: RefCell<[Vec<u8>; 2]> = RefCell::new([vec![0; CHUNK], vec![0; CHUNK]]);
}

/// Whether files `a` and `b`, both `len` bytes long when stat'ed, hold the
/// same bytes, read side by side. Reading stops at `len`, which saves the
/// read that finds the end on each of thousands of small files; a file
/// that grew since is caught by the next refresh.
fn same_bytes(a: &Path, b: &Path, len: u64) -> io::Result<bool> {
    let (mut a, mut b) = (File::open(a)?, File::open(b)?);
    BUFFERS.with_borrow_mut(|[a_buf, b_buf]| {
        let mut left = len;
        while left > 0 {
            let want = usize::try_from(left).map_or(CHUNK, |left| left.min(CHUNK));
            let n = read_full(&mut a, &mut a_buf[..want])?;
            let m = read_full(&mut b, &mut b_buf[..want])?;
            if n != m || a_buf[..n] != b_buf[..m] {
                return Ok(false);
            }
            if n < want {
                return Ok(true);
            }
            left -= n as u64;
        }
        Ok(true)
    })
}

/// Fills `buf` unless the file ends first; returns how much was read.
fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

struct Entry {
    /// The name, with a `/` after a directory's: sorting by it gives git's
    /// order, where `a/b` comes after `a.c` and before `a0`.
    key: Vec<u8>,
    path: PathBuf,
    kind: Kind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
    /// A symlink whose target is missing.
    Dangling,
}

impl Entry {
    fn name(&self) -> &[u8] {
        match self.kind {
            Kind::Dir => &self.key[..self.key.len() - 1],
            Kind::File | Kind::Dangling => &self.key,
        }
    }
}

/// Two directory trees walked in step, depth first, both sides' entries
/// merged in git's order.
struct Walk<'a> {
    pair: &'a Pair,
    filter: &'a Filter,
    sink: &'a mut dyn FnMut(Found) -> ControlFlow<()>,
    /// Per side, the directories being walked, by device and inode, so a
    /// symlink back up the tree isn't followed in circles.
    ancestors: [Vec<(u64, u64)>; 2],
    /// Per side with `--gitignore`, the `.gitignore` of each directory being
    /// walked, innermost last.
    rules: [Vec<Gitignore>; 2],
}

/// A directory being walked: what's left of both sides' entries.
struct Frame {
    lists: [std::iter::Peekable<std::vec::IntoIter<Entry>>; 2],
    /// The sides whose ancestors and rules this directory added to.
    entered: [bool; 2],
    /// How long the walk's relative path was before this directory's name.
    parent_len: usize,
}

impl Walk<'_> {
    /// Walks the two top directories, `dirs`. The directories being walked
    /// are kept on a stack of their own rather than the thread's, so a tree
    /// as deep as paths go doesn't overflow it.
    fn run(&mut self, dirs: [Option<&Path>; 2]) -> Result<ControlFlow<()>> {
        let mut rel = BString::default();
        let mut stack = vec![self.enter(dirs, rel.as_bstr(), 0)?];
        while let Some(frame) = stack.last_mut() {
            let [olds, news] = &mut frame.lists;
            let step = match (olds.peek(), news.peek()) {
                (None, None) => None,
                (Some(o), Some(n)) if o.key == n.key => Some((true, true)),
                (Some(o), Some(n)) if o.key < n.key => Some((true, false)),
                (Some(_), None) => Some((true, false)),
                (_, Some(_)) => Some((false, true)),
            };
            let Some((take_old, take_new)) = step else {
                let (entered, parent_len) = (frame.entered, frame.parent_len);
                stack.pop();
                self.leave(entered);
                rel.truncate(parent_len);
                continue;
            };
            let old = olds.next_if(|_| take_old);
            let new = news.next_if(|_| take_new);
            let Some(entry) = old.as_ref().or(new.as_ref()) else {
                continue;
            };
            let len = rel.len();
            if len > 0 {
                rel.push(b'/');
            }
            rel.extend_from_slice(entry.name());
            if entry.kind == Kind::Dir {
                if self.filter.may_contain(&rel) {
                    let dirs = [&old, &new].map(|e| e.as_ref().map(|e| e.path.as_path()));
                    let frame = self.enter(dirs, rel.as_bstr(), len)?;
                    // The path stays on until the directory is left.
                    stack.push(frame);
                    continue;
                }
            } else if self.filter.matches(&rel)
                && self
                    .file(rel.as_bstr(), old.as_ref(), new.as_ref())
                    .is_break()
            {
                return Ok(ControlFlow::Break(()));
            }
            rel.truncate(len);
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Starts the directory `rel` (empty for the top), which is at `dirs` on
    /// each side that has it, `parent_len` being where its name starts in
    /// `rel`. A side whose directory is already being walked, through a
    /// symlink back up the tree, has no entries.
    fn enter(&mut self, dirs: [Option<&Path>; 2], rel: &BStr, parent_len: usize) -> Result<Frame> {
        let mut entered = [false; 2];
        let mut lists: [Vec<Entry>; 2] = [Vec::new(), Vec::new()];
        for side in 0..2 {
            let Some(dir) = dirs[side] else { continue };
            let meta = fs::metadata(dir).map_err(io_error(dir))?;
            let id = (meta.dev(), meta.ino());
            if self.ancestors[side].contains(&id) {
                continue;
            }
            self.ancestors[side].push(id);
            if self.pair.gitignore {
                self.rules[side].push(Gitignore::new(dir.join(".gitignore")).0);
            }
            entered[side] = true;
            lists[side] = self.list(dir, rel, side)?;
        }
        Ok(Frame {
            lists: lists.map(|list| list.into_iter().peekable()),
            entered,
            parent_len,
        })
    }

    /// Done with a directory [`Walk::enter`] started.
    fn leave(&mut self, entered: [bool; 2]) {
        for (side, _) in entered.iter().enumerate().filter(|(_, entered)| **entered) {
            self.ancestors[side].pop();
            if self.pair.gitignore {
                self.rules[side].pop();
            }
        }
    }

    /// The entries of `dir` (at `rel` on `side`) that the walk takes, in
    /// git's order.
    fn list(&self, dir: &Path, rel: &BStr, side: usize) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        let mut path = BString::from(rel.to_vec());
        for entry in fs::read_dir(dir).map_err(io_error(dir))? {
            let entry = entry.map_err(io_error(dir))?;
            let full = entry.path();
            let file_type = entry.file_type().map_err(io_error(&full))?;
            let kind = if file_type.is_dir() {
                Kind::Dir
            } else if file_type.is_file() {
                Kind::File
            } else if file_type.is_symlink() {
                match fs::metadata(&full) {
                    Ok(meta) if meta.is_dir() => Kind::Dir,
                    Ok(meta) if meta.is_file() => Kind::File,
                    Ok(_) => continue,
                    Err(_) => Kind::Dangling,
                }
            } else {
                continue;
            };
            let name = entry.file_name();
            let is_dir = kind == Kind::Dir;
            path.truncate(rel.len());
            if !rel.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(name.as_bytes());
            let excluded = self
                .pair
                .exclude
                .matched(Path::new(OsStr::from_bytes(&path)), is_dir)
                .is_ignore();
            if excluded || (self.pair.gitignore && self.ignored(side, &full, &name, is_dir)) {
                continue;
            }
            let mut key = name.into_vec();
            if is_dir {
                key.push(b'/');
            }
            entries.push(Entry {
                key,
                path: full,
                kind,
            });
        }
        entries.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        Ok(entries)
    }

    /// Whether `.git` or the nearest `.gitignore` rule on `side` leaves
    /// `full` out.
    fn ignored(&self, side: usize, full: &Path, name: &OsStr, is_dir: bool) -> bool {
        if name == ".git" {
            return true;
        }
        for rules in self.rules[side].iter().rev() {
            match rules.matched(full, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        false
    }

    /// Hands the file `rel`, at `old` and `new` on the sides that have it,
    /// to the workers.
    fn file(&mut self, rel: &BStr, old: Option<&Entry>, new: Option<&Entry>) -> ControlFlow<()> {
        let side = |entry: Option<&Entry>| entry.map(|e| (e.path.clone(), e.kind));
        (self.sink)(Found {
            rel: rel.to_owned(),
            sides: [side(old), side(new)],
        })
    }
}

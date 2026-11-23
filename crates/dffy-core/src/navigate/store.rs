//! Where a process keeps the definitions its navigators hold: one file in
//! the symbol cache's directory, unlinked as soon as it's made, that each
//! file's encoded definitions are appended to as the index is built and
//! read back by position. Freed memory stays with the process that had
//! it, so the definitions live in the file rather than the heap; the pages
//! read often stay in the system's file cache, which no process is charged
//! for. The file goes when the process exits.
//!
//! A built index's postings, each name's hash with the files defining it,
//! go to the same file in blocks of [`BLOCK`] names, with one hash and
//! place per block kept in memory.
//!
//! When the file can't be made or written (a full disk), what comes after
//! is kept in memory, and [`Store::note`] says so.

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rustc_hash::FxHashMap;

use crate::cache;

/// Names per block of postings.
const BLOCK: usize = 64;

/// Bytes of the entries read last kept in memory, so a lookup asked again
/// doesn't read them again.
const RECENT_BYTES: usize = 2 << 20;

/// Store files made in this process, for their names.
static STORES: AtomicU64 = AtomicU64::new(0);

/// Where an entry is in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Locator {
    offset: u64,
    len: u32,
}

impl Locator {
    /// Its bytes in the store.
    pub(super) fn len(self) -> u64 {
        u64::from(self.len)
    }
}

/// The process's store of definitions. Its methods take `&self`; build
/// threads append while lookups read.
#[derive(Debug, Default)]
pub(super) struct Store(Mutex<Inner>);

#[derive(Debug, Default)]
struct Inner {
    /// Made on the first append.
    file: Option<Arc<File>>,
    tried: bool,
    end: u64,
    /// Where the bytes kept in memory start, once the file failed: every
    /// offset from it on is in `memory`.
    memory_from: Option<u64>,
    memory: Vec<u8>,
    note: Option<String>,
    recent: Recent,
}

impl Store {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        // Nothing panics while it's held half changed.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Appends `bytes`, making the file in `dir` on the first call. A
    /// write that fails keeps them, and all after them, in memory.
    pub(super) fn append(&self, dir: &Path, bytes: &[u8]) -> Locator {
        let mut inner = self.inner();
        if !inner.tried {
            inner.tried = true;
            let offset = inner.end;
            match make(dir) {
                Ok(file) => inner.file = Some(Arc::new(file)),
                Err(e) => inner.keep_in_memory(offset, &e),
            }
        }
        inner.append(bytes)
    }

    /// The bytes at `at`; `None` when they can't be read whole.
    pub(super) fn read(&self, at: Locator) -> Option<Arc<[u8]>> {
        let file = {
            let inner = self.inner();
            if let Some(bytes) = inner.recent.get(at.offset) {
                return Some(bytes);
            }
            if let Some(from) = inner.memory_from
                && at.offset >= from
            {
                let start = usize::try_from(at.offset - from).ok()?;
                let bytes: Arc<[u8]> = inner.memory.get(start..start + at.len as usize)?.into();
                return Some(bytes);
            }
            Arc::clone(inner.file.as_ref()?)
        };
        let mut bytes = vec![0; at.len as usize];
        file.read_exact_at(&mut bytes, at.offset).ok()?;
        let bytes: Arc<[u8]> = bytes.into();
        self.inner().recent.put(at.offset, Arc::clone(&bytes));
        Some(bytes)
    }

    /// Why definitions are kept in memory rather than in the file, if they
    /// are.
    pub(super) fn note(&self) -> Option<String> {
        self.inner().note.clone()
    }

    /// Bytes appended so far, in the file and in memory.
    pub(super) fn len(&self) -> u64 {
        self.inner().end
    }

    /// Cuts the file short, as a failing disk would leave it, for tests.
    #[cfg(test)]
    pub(super) fn damage(&self) {
        let mut inner = self.inner();
        if let Some(file) = &inner.file {
            file.set_len(0).ok();
        }
        inner.recent = Recent::default();
    }

    /// Whether anything was stored yet: postings go only where definitions
    /// went.
    pub(super) fn ready(&self) -> bool {
        self.inner().tried
    }

    /// Writes `postings`, sorted by hash then file, as blocks.
    pub(super) fn put_postings(&self, postings: impl IntoIterator<Item = (u32, u32)>) -> Postings {
        let mut blocks = Vec::new();
        let mut block = Block::default();
        let mut group: Option<(u32, Vec<u32>)> = None;
        for (hash, file) in postings {
            match &mut group {
                Some((h, files)) if *h == hash => {
                    if files.last() != Some(&file) {
                        files.push(file);
                    }
                }
                _ => {
                    if let Some((h, files)) = group.replace((hash, vec![file])) {
                        block.add(h, &files);
                        if block.groups == BLOCK {
                            blocks.extend(std::mem::take(&mut block).put(self));
                        }
                    }
                }
            }
        }
        if let Some((h, files)) = group {
            block.add(h, &files);
        }
        blocks.extend(block.put(self));
        Postings { blocks }
    }

    /// `runs` and `fresh`, each sorted, merged and written as one run;
    /// `None` when a run can't be read back.
    pub(super) fn merge_postings(
        &self,
        runs: &[Postings],
        fresh: impl Iterator<Item = (u32, u32)>,
    ) -> Option<Postings> {
        let mut cursors: Vec<Cursor> = runs.iter().map(Cursor::new).collect();
        let mut heap = std::collections::BinaryHeap::new();
        for (i, cursor) in cursors.iter_mut().enumerate() {
            if let Some(p) = cursor.next(self).ok()? {
                heap.push(std::cmp::Reverse((p, i)));
            }
        }
        let mut fresh = fresh.peekable();
        let mut failed = false;
        let merged = std::iter::from_fn(|| {
            let from_runs = heap.peek().map(|std::cmp::Reverse((p, _))| *p);
            match (from_runs, fresh.peek()) {
                (Some(r), Some(&f)) if f < r => fresh.next(),
                (Some(_), _) => {
                    let std::cmp::Reverse((p, i)) = heap.pop()?;
                    match cursors[i].next(self) {
                        Ok(Some(next)) => heap.push(std::cmp::Reverse((next, i))),
                        Ok(None) => {}
                        Err(Unreadable) => failed = true,
                    }
                    Some(p)
                }
                (None, _) => fresh.next(),
            }
        });
        let run = self.put_postings(merged);
        (!failed).then_some(run)
    }

    /// The files posted under `hash` in `run`, ascending; `None` when its
    /// block can't be read.
    pub(super) fn posted(&self, run: &Postings, hash: u32) -> Option<Vec<u32>> {
        let block = run.blocks.partition_point(|b| b.0 <= hash);
        let Some(&(_, at)) = block.checked_sub(1).and_then(|b| run.blocks.get(b)) else {
            return Some(Vec::new());
        };
        let postings = decode_block(&self.read(at)?)?;
        Some(
            (postings.into_iter())
                .filter(|p| p.0 == hash)
                .map(|p| p.1)
                .collect(),
        )
    }
}

impl Inner {
    fn append(&mut self, bytes: &[u8]) -> Locator {
        let offset = self.end;
        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        let bytes = &bytes[..len as usize];
        let written = match (&self.file, self.memory_from) {
            (Some(file), None) => file.write_all_at(bytes, offset),
            _ => Ok(()),
        };
        if let Err(e) = written {
            self.keep_in_memory(offset, &e);
        }
        if self.memory_from.is_some() {
            self.memory.extend_from_slice(bytes);
        }
        self.end += u64::from(len);
        Locator { offset, len }
    }

    /// Keeps what's appended from `offset` on in memory.
    fn keep_in_memory(&mut self, offset: u64, e: &io::Error) {
        self.memory_from.get_or_insert(offset);
        self.note.get_or_insert_with(|| {
            format!("the definitions index is kept in memory: the symbol cache can't take it ({e})")
        });
    }
}

/// Postings in the store: each block's first hash and where the block is.
#[derive(Debug)]
pub(super) struct Postings {
    blocks: Vec<(u32, Locator)>,
}

impl Postings {
    /// Their bytes in the store.
    pub(super) fn len(&self) -> u64 {
        self.blocks.iter().map(|(_, at)| at.len()).sum()
    }
}

/// Makes the store's file in `dir` and unlinks it: it lives as long as
/// the process holds it.
fn make(dir: &Path) -> io::Result<File> {
    std::fs::create_dir_all(dir)?;
    let n = STORES.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("navigate-{}-{n}", std::process::id()));
    let file = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::fs::remove_file(&path)?;
    Ok(file)
}

/// The entries read last, up to [`RECENT_BYTES`], the oldest dropped
/// first.
#[derive(Debug, Default)]
struct Recent {
    by_offset: FxHashMap<u64, Arc<[u8]>>,
    order: VecDeque<u64>,
    bytes: usize,
}

impl Recent {
    fn get(&self, offset: u64) -> Option<Arc<[u8]>> {
        self.by_offset.get(&offset).cloned()
    }

    fn put(&mut self, offset: u64, bytes: Arc<[u8]>) {
        if bytes.len() > RECENT_BYTES / 4 {
            return;
        }
        self.bytes += bytes.len();
        match self.by_offset.insert(offset, bytes) {
            Some(old) => self.bytes -= old.len(),
            None => self.order.push_back(offset),
        }
        while self.bytes > RECENT_BYTES
            && let Some(old) = self.order.pop_front()
        {
            if let Some(gone) = self.by_offset.remove(&old) {
                self.bytes -= gone.len();
            }
        }
    }
}

/// A block of postings being written: each name's hash as a step from the
/// one before, how many files have it, and the files as steps.
#[derive(Default)]
struct Block {
    bytes: Vec<u8>,
    first: Option<u32>,
    previous: u32,
    groups: usize,
}

impl Block {
    fn add(&mut self, hash: u32, files: &[u32]) {
        self.first.get_or_insert(hash);
        cache::put_varint(&mut self.bytes, u64::from(hash - self.previous));
        self.previous = hash;
        cache::put_varint(&mut self.bytes, files.len() as u64);
        let mut previous = 0;
        for &f in files {
            cache::put_varint(&mut self.bytes, u64::from(f - previous));
            previous = f;
        }
        self.groups += 1;
    }

    fn put(self, store: &Store) -> Option<(u32, Locator)> {
        let first = self.first?;
        Some((first, store.inner().append(&self.bytes)))
    }
}

/// A run's postings read back in order, a block at a time.
struct Cursor<'r> {
    blocks: std::slice::Iter<'r, (u32, Locator)>,
    read: std::vec::IntoIter<(u32, u32)>,
}

impl<'r> Cursor<'r> {
    fn new(run: &'r Postings) -> Self {
        Self {
            blocks: run.blocks.iter(),
            read: Vec::new().into_iter(),
        }
    }

    /// The next posting, `None` past the last; an error when a block can't
    /// be read.
    fn next(&mut self, store: &Store) -> Result<Option<(u32, u32)>, Unreadable> {
        loop {
            if let Some(p) = self.read.next() {
                return Ok(Some(p));
            }
            let Some(&(_, at)) = self.blocks.next() else {
                return Ok(None);
            };
            let block = store.read(at).and_then(|bytes| decode_block(&bytes));
            self.read = block.ok_or(Unreadable)?.into_iter();
        }
    }
}

/// A block of postings that can't be read back.
#[derive(Debug)]
struct Unreadable;

impl Cursor<'_> {}

/// A block's postings, in order; `None` when it doesn't decode.
fn decode_block(mut input: &[u8]) -> Option<Vec<(u32, u32)>> {
    let mut out = Vec::new();
    let mut hash = 0u32;
    while !input.is_empty() {
        hash = hash.checked_add(u32::try_from(cache::get_varint(&mut input)?).ok()?)?;
        let count = cache::get_varint(&mut input)?;
        let mut file = 0u32;
        for _ in 0..count {
            file = file.checked_add(u32::try_from(cache::get_varint(&mut input)?).ok()?)?;
            out.push((hash, file));
        }
    }
    Some(out)
}

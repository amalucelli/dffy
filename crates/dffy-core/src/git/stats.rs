//! Line counts kept in the disk cache by the contents they're of, so that a
//! change list counted once is counted at once the next time, in any
//! process and whatever part of it is asked for: the viewer's file tree,
//! `--stat`, JSON summaries, a page of the MCP server's file list.
//!
//! Unlike the cache's other entries, these are shared and rewritten. A
//! record holds the ids of a change's two sides and their [`Stat`] under one
//! whitespace mode. Records are spread by id over [`SHARDS`] entries per
//! mode, each read once per process on first use. Writing a shard reads it
//! again and adds the records counted since, keeping the newest within
//! [`SHARD_BYTES`], since a shard in use never ages out of the cache. A
//! record never changes meaning, so a write lost to another process's costs
//! only a recount. Records that took less than [`WORTH_WRITING`] to count
//! in all aren't written, and a store that has counted for less than
//! [`READ_AFTER`] reads nothing.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap, FxHashSet};

use super::{FileChange, MAX_DIFF_BYTES, Mode, ObjectId, Repo, Result, Side, Stat};
use crate::cache::{self, Cache, Kind};
use crate::diff::Whitespace;

/// Part of every shard's key. Bumped when the record layout changes, and
/// when the line diff would count a change differently: the crate's version,
/// also in the key, doesn't change with every such commit.
const VERSION: u8 = 1;
/// Shards per whitespace mode. A process reads, and after counting
/// rewrites, each shard its files fall in: a list of a few dozen files
/// touches all of them, which keeps what a list's first count pays in file
/// writes small.
const SHARDS: usize = 16;
/// About 5,700 records per shard, 90,000 per whitespace mode. It also
/// bounds what a process holds and decodes per shard it looks in, since it
/// reads a shard whole.
const SHARD_BYTES: usize = 256 << 10;
const MODES: usize = 3;
/// How long the records not written yet must have taken to count before a
/// flush writes them: long enough that rewriting every shard is small next
/// to it, so a list that counts quickly runs as fast as without the cache.
const WORTH_WRITING: Duration = Duration::from_millis(100);
/// How long a store counts before it reads shards: a list that counts in
/// less reads nothing, since reading its shards would cost about as much.
const READ_AFTER: Duration = Duration::from_millis(2);
/// Lookups that may find nothing in the shards read, before any finds
/// something, until a store stops reading: a list whose counts were never
/// kept reads only a few shards.
const MISSES: u32 = 3;

/// A side's content as a record names it: a blob, or a submodule's commit,
/// which reads as a line naming it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Content {
    id: ObjectId,
    gitlink: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Pair {
    old: Option<Content>,
    new: Option<Content>,
}

impl Pair {
    /// `None` when a side has no id without reading it: a working-tree file
    /// the status scan didn't hash, or a file compared outside git.
    fn of(change: &FileChange) -> Option<Self> {
        let content = |side: Option<Side>| match side {
            None => Some(None),
            Some(Side::Object { id, mode } | Side::Worktree { id: Some(id), mode }) => {
                Some(Some(Content {
                    id,
                    gitlink: mode == Mode::Gitlink,
                }))
            }
            Some(Side::Worktree { id: None, .. } | Side::Path { .. }) => None,
        };
        let pair = Self {
            old: content(change.old)?,
            new: content(change.new)?,
        };
        (pair.old.is_some() || pair.new.is_some()).then_some(pair)
    }

    fn shard(&self) -> usize {
        self.new
            .or(self.old)
            .map_or(0, |c| usize::from(c.id.first_byte()) % SHARDS)
    }
}

/// Counts changes through the disk cache. Shared by threads; what they
/// count is written by [`StatStore::flush`], or when the store is dropped,
/// once it took long enough to count to be worth the write.
pub struct StatStore {
    cache: Cache,
    /// [`SHARDS`] per whitespace mode.
    shards: Box<[Mutex<Shard>]>,
    /// Held through a flush, so one that finds another under way returns
    /// only once that one's written.
    flushing: Mutex<()>,
    /// How long the records not written yet took to count, in nanoseconds.
    counting: AtomicU64,
    /// How long everything this store counted took, in nanoseconds.
    counted: AtomicU64,
    /// A lookup in a shard read from the cache found its pair.
    hit: AtomicBool,
    /// Lookups that found nothing, with reading on.
    misses: AtomicU32,
    /// [`WORTH_WRITING`] and [`READ_AFTER`], but for tests.
    worth_writing: Duration,
    read_after: Duration,
}

#[derive(Default)]
struct Shard {
    /// Counted by this process, and once `read`, what the cache had.
    known: FxHashMap<Pair, Stat>,
    read: bool,
    /// Counted since the shard was last written.
    new: Vec<(Pair, Stat)>,
}

impl StatStore {
    /// A store over `cache`; nothing is read until a count is asked for.
    #[must_use]
    pub fn new(cache: Cache) -> Self {
        Self {
            cache,
            shards: (0..MODES * SHARDS).map(|_| Mutex::default()).collect(),
            flushing: Mutex::new(()),
            counting: AtomicU64::new(0),
            counted: AtomicU64::new(0),
            hit: AtomicBool::new(false),
            misses: AtomicU32::new(0),
            worth_writing: WORTH_WRITING,
            read_after: READ_AFTER,
        }
    }

    /// [`Repo::stat`], from the cache when these contents were counted
    /// before under `whitespace`. Failures and files too large to diff
    /// aren't kept.
    ///
    /// # Errors
    ///
    /// See [`Repo::stat`].
    pub fn stat(&self, repo: &Repo, change: &FileChange, whitespace: Whitespace) -> Result<Stat> {
        let Some(pair) = Pair::of(change) else {
            return repo.stat(change, whitespace);
        };
        let slot = mode(whitespace) * SHARDS + pair.shard();
        if let Some(stat) = self.find(slot, &pair) {
            return Ok(stat);
        }
        let started = Instant::now();
        let stat = repo.stat(change, whitespace)?;
        let took = nanos(started.elapsed());
        self.counted.fetch_add(took, Ordering::Relaxed);
        // Counted from the sizes alone, without reading the contents.
        let sized = stat
            .binary
            .is_some_and(|(old, new)| old.max(new) >= MAX_DIFF_BYTES);
        if !sized {
            let mut shard = self.lock(slot);
            shard.known.insert(pair, stat);
            shard.new.push((pair, stat));
            self.counting.fetch_add(took, Ordering::Relaxed);
        }
        Ok(stat)
    }

    /// `pair`'s stat if this process counted it or, while reading is worth
    /// it, the cache has it.
    fn find(&self, slot: usize, pair: &Pair) -> Option<Stat> {
        let mut shard = self.lock(slot);
        let worth_reading = self.hit.load(Ordering::Relaxed)
            || self.counted.load(Ordering::Relaxed) >= nanos(self.read_after)
                && self.misses.load(Ordering::Relaxed) < MISSES;
        if !worth_reading {
            return shard.known.get(pair).copied();
        }
        // Read under the lock, so threads wanting the same shard read it once.
        if !shard.read {
            shard.read = true;
            if let Some(records) = self.read(slot) {
                shard.known.extend(records);
            }
        }
        let found = shard.known.get(pair).copied();
        if found.is_some() {
            self.hit.store(true, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        found
    }

    /// Writes the shards that gained records since they were last written,
    /// a thread per core, once those records took `WORTH_WRITING` to
    /// count; until then they're kept for a later flush.
    pub fn flush(&self) {
        let _flushing = self.flushing.lock().unwrap_or_else(PoisonError::into_inner);
        if self.counting.load(Ordering::Relaxed) < nanos(self.worth_writing) {
            return;
        }
        self.counting.store(0, Ordering::Relaxed);
        let mut work: Vec<(usize, Vec<(Pair, Stat)>)> = (0..self.shards.len())
            .map(|slot| (slot, std::mem::take(&mut self.lock(slot).new)))
            .filter(|(_, new)| !new.is_empty())
            .collect();
        let threads = std::thread::available_parallelism()
            .map_or(1, std::num::NonZero::get)
            .min(work.len());
        if threads == 0 {
            return;
        }
        let per_thread = work.len().div_ceil(threads);
        std::thread::scope(|scope| {
            for part in work.chunks_mut(per_thread) {
                scope.spawn(|| {
                    for (slot, new) in part {
                        self.write(*slot, std::mem::take(new));
                    }
                });
            }
        });
    }

    /// Rewrites shard `slot` with `new` added.
    fn write(&self, slot: usize, new: Vec<(Pair, Stat)>) {
        let added: FxHashSet<Pair> = new.iter().map(|r| r.0).collect();
        // Read again: another process may have written it since.
        let mut records = self.read(slot).unwrap_or_default();
        records.retain(|r| !added.contains(&r.0));
        records.extend(new);
        let mut bytes = Vec::new();
        let mut starts = Vec::with_capacity(records.len());
        for record in &records {
            starts.push(bytes.len());
            encode(record, &mut bytes);
        }
        let keep_from = starts
            .into_iter()
            .find(|&start| bytes.len() - start <= SHARD_BYTES)
            .unwrap_or(bytes.len());
        bytes.drain(..keep_from);
        self.cache.put(Kind::Stats, &key(slot), &bytes);
    }

    fn lock(&self, slot: usize) -> MutexGuard<'_, Shard> {
        self.shards[slot]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The shard's records, oldest first; `None` when it has none or they
    /// don't decode.
    fn read(&self, slot: usize) -> Option<Vec<(Pair, Stat)>> {
        let bytes = self.cache.get(Kind::Stats, &key(slot))?;
        let mut input = &bytes[..];
        let mut records = Vec::new();
        while !input.is_empty() {
            records.push(decode(&mut input)?);
        }
        Some(records)
    }
}

impl Drop for StatStore {
    fn drop(&mut self) {
        self.flush();
    }
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn mode(whitespace: Whitespace) -> usize {
    match whitespace {
        Whitespace::Exact => 0,
        Whitespace::IgnoreAll => 1,
        Whitespace::IgnoreChange => 2,
    }
}

fn key(slot: usize) -> [u8; 3] {
    let [.., hi, lo] = slot.to_be_bytes();
    [VERSION, hi, lo]
}

const OLD: u8 = 1;
const NEW: u8 = 1 << 1;
const OLD_GITLINK: u8 = 1 << 2;
const NEW_GITLINK: u8 = 1 << 3;
const BINARY: u8 = 1 << 4;
/// SHA-256 ids; SHA-1 otherwise.
const LONG_IDS: u8 = 1 << 5;

/// Flags, the ids present, then the line counts, or for a binary file both
/// sides' sizes.
fn encode((pair, stat): &(Pair, Stat), out: &mut Vec<u8>) {
    let mut flags = 0;
    for (side, present, gitlink) in [(pair.old, OLD, OLD_GITLINK), (pair.new, NEW, NEW_GITLINK)] {
        if let Some(c) = side {
            flags |= present;
            if c.gitlink {
                flags |= gitlink;
            }
            if c.id.kind() != gix::hash::Kind::Sha1 {
                flags |= LONG_IDS;
            }
        }
    }
    if stat.binary.is_some() {
        flags |= BINARY;
    }
    out.push(flags);
    for c in [pair.old, pair.new].into_iter().flatten() {
        out.extend_from_slice(c.id.as_bytes());
    }
    let (a, b) = stat
        .binary
        .unwrap_or((stat.added as u64, stat.removed as u64));
    cache::put_varint(out, a);
    cache::put_varint(out, b);
}

fn decode(input: &mut &[u8]) -> Option<(Pair, Stat)> {
    let (&flags, rest) = input.split_first()?;
    *input = rest;
    let len = if flags & LONG_IDS == 0 { 20 } else { 32 };
    let mut side = |present: u8, gitlink: u8| -> Option<Option<Content>> {
        if flags & present == 0 {
            return Some(None);
        }
        let (id, rest) = input.split_at_checked(len)?;
        *input = rest;
        Some(Some(Content {
            id: ObjectId::try_from(id).ok()?,
            gitlink: flags & gitlink != 0,
        }))
    };
    let pair = Pair {
        old: side(OLD, OLD_GITLINK)?,
        new: side(NEW, NEW_GITLINK)?,
    };
    let (a, b) = (cache::get_varint(input)?, cache::get_varint(input)?);
    let stat = if flags & BINARY == 0 {
        Stat {
            added: usize::try_from(a).ok()?,
            removed: usize::try_from(b).ok()?,
            binary: None,
        }
    } else {
        Stat {
            binary: Some((a, b)),
            ..Stat::default()
        }
    };
    Some((pair, stat))
}

#[cfg(test)]
mod tests;

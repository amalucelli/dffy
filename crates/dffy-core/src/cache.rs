//! A content-addressed disk cache shared by every dffy process of a user,
//! for results that take far longer to compute than to read back. An entry's
//! name is a hash of the dffy-core version, its kind and every input it depends
//! on, so entries never need invalidating; old ones age out under the size
//! budget.
//!
//! Writes go to a temporary file renamed into place, so readers in other
//! processes see a whole entry or none. Each entry carries its length and a
//! checksum; anything that fails to decode is removed and treated as a miss.

use std::fs;
use std::hash::Hasher as _;
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use rustc_hash::FxHasher;

use crate::git::ObjectId;

/// Bumped when the entry layout changes; part of every key, so old entries
/// are never read again.
const FORMAT: u8 = 1;
const MAGIC: &[u8; 4] = b"dffy";
/// Magic, format, kind, payload length, checksum.
const HEADER: usize = 4 + 1 + 1 + 8 + 8;

/// How often a process may scan the cache to enforce its budget. The scan
/// stats every entry, so it runs on its own thread and at most this often
/// across all processes (a stamp file records the last one).
const EVICT_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Eviction removes the oldest entries until the cache is this fraction of
/// its budget, so the next scan isn't due right away.
const EVICT_TO_PERCENT: u64 = 80;

/// What an entry holds. Part of its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// A side's whole-file highlight spans.
    Highlight = 1,
    /// A rename pass over a list of additions and deletions.
    Renames = 2,
    /// A file's structural diff.
    Structural = 3,
    /// A renderer plugin's answer for a file.
    Rendered = 4,
    /// A shard of line counts by content (see [`crate::git::StatStore`]):
    /// unlike the other kinds, rewritten as records are added.
    Stats = 5,
    /// A review plugin's `describe` answer.
    Described = 6,
    /// The names of the plugins of one kind found on PATH.
    Plugins = 7,
    /// A blob's definitions (see [`crate::highlight::blob_definitions`]),
    /// kept in [`Cache::symbols`].
    Symbols = 8,
}

/// The budget of [`Cache::symbols`]: the kernel's v6.0 and v6.1 together,
/// with room for as much again.
pub const SYMBOLS_BUDGET: u64 = 128 << 20;

/// A cache directory with a size budget.
#[derive(Debug, Clone)]
pub struct Cache {
    dir: PathBuf,
    budget: u64,
    /// Entries count the blocks they take on disk rather than their
    /// length.
    blocks: bool,
}

/// The cache of definitions, [`Cache::symbols`]: its own directory and
/// budget, apart from the cache it's made from.
#[derive(Debug, Clone)]
pub struct SymbolCache(Cache);

impl SymbolCache {
    /// The cache underneath, to read its usage or budget.
    #[must_use]
    pub fn as_cache(&self) -> &Cache {
        &self.0
    }

    /// The budget in bytes on disk.
    #[must_use]
    pub fn budget(&self) -> u64 {
        self.0.budget
    }

    /// Bytes the entries take on disk, and how many there are.
    ///
    /// # Errors
    ///
    /// When the directory exists but can't be read.
    pub fn usage(&self) -> io::Result<(u64, usize)> {
        self.0.usage()
    }

    /// Each entry's bytes on disk, with whether it's the entry of `kind`
    /// for `key`.
    pub(crate) fn sizes(&self, kind: Kind, key: &[u8]) -> io::Result<Vec<(u64, bool)>> {
        let own = self.0.path(kind, key);
        Ok((self.0.entries()?.into_iter())
            .map(|(path, size, _)| (size, path == own))
            .collect())
    }
}

impl Cache {
    /// A cache in `dir`, created on the first write, kept under `budget`
    /// bytes by eviction.
    #[must_use]
    pub fn new(dir: PathBuf, budget: u64) -> Self {
        Self {
            dir,
            budget,
            blocks: false,
        }
    }

    /// `$XDG_CACHE_HOME/dffy`, falling back to `~/.cache/dffy`.
    #[must_use]
    pub fn default_dir() -> Option<PathBuf> {
        let xdg = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        let base =
            xdg.or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
        Some(base.join("dffy"))
    }

    /// The cache for [`Kind::Symbols`]: a directory inside this one with
    /// its own budget, [`SYMBOLS_BUDGET`] or this cache's if smaller. A
    /// revision's definitions are tens of thousands of entries, which in a
    /// shared budget would evict every highlight; this cache's scans don't
    /// see them, and theirs don't see this cache's. Its entries are few
    /// and large, and count the blocks they take on disk.
    #[must_use]
    pub fn symbols(&self) -> SymbolCache {
        SymbolCache(Self {
            blocks: true,
            ..Self::new(self.dir.join("symbols"), SYMBOLS_BUDGET.min(self.budget))
        })
    }

    /// The budget in bytes on disk.
    #[must_use]
    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// Where the entries live; it may not exist yet.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, kind: Kind, key: &[u8]) -> PathBuf {
        let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
        hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
        hasher.update(&[0, FORMAT, kind as u8]);
        hasher.update(key);
        let name = hasher
            .try_finalize()
            .map_or_else(|_| "0".repeat(40), |id| id.to_hex().to_string());
        self.dir.join(&name[..2]).join(&name[2..])
    }

    /// The entry of `kind` for `key`, if one exists and is intact. A hit
    /// refreshes the entry's age, so eviction drops the least recently used.
    #[must_use]
    pub fn get(&self, kind: Kind, key: &[u8]) -> Option<Vec<u8>> {
        let path = self.path(kind, key);
        let mut bytes = fs::read(&path).ok()?;
        if !intact(&bytes, kind) {
            fs::remove_file(&path).ok();
            return None;
        }
        if let Ok(file) = fs::File::options().append(true).open(&path) {
            file.set_modified(SystemTime::now()).ok();
        }
        bytes.drain(..HEADER);
        Some(bytes)
    }

    /// Stores `value` as the entry of `kind` for `key`. Failures are
    /// ignored: the cache is only ever an accelerator.
    pub fn put(&self, kind: Kind, key: &[u8], value: &[u8]) {
        write_atomic(&self.path(kind, key), kind, value).ok();
        self.maybe_evict();
    }

    /// Bytes the entries take, and how many there are.
    ///
    /// # Errors
    ///
    /// When the directory exists but can't be read.
    pub fn usage(&self) -> io::Result<(u64, usize)> {
        let entries = self.entries()?;
        Ok((entries.iter().map(|e| e.1).sum(), entries.len()))
    }

    /// Every entry with its size and modification time.
    fn entries(&self) -> io::Result<Vec<(PathBuf, u64, SystemTime)>> {
        let mut out = Vec::new();
        let shards = match fs::read_dir(&self.dir) {
            Ok(shards) => shards,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e),
        };
        for shard in shards.flatten() {
            if shard.file_name().len() != 2 {
                continue;
            }
            let Ok(files) = fs::read_dir(shard.path()) else {
                continue;
            };
            for file in files.flatten() {
                if let Ok(meta) = file.metadata()
                    && meta.is_file()
                {
                    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    let size = if self.blocks {
                        meta.blocks() * 512
                    } else {
                        meta.len()
                    };
                    out.push((file.path(), size, modified));
                }
            }
        }
        Ok(out)
    }

    /// When the cache is over its budget, removes the least recently used
    /// entries until it's under 80% of it. Temporary files left by crashed
    /// writers go too.
    ///
    /// # Errors
    ///
    /// When the directory can't be read.
    pub fn evict(&self) -> io::Result<()> {
        let mut entries = self.entries()?;
        let total: u64 = entries.iter().map(|e| e.1).sum();
        let stale = SystemTime::now() - EVICT_INTERVAL;
        for (path, _, modified) in &entries {
            if is_temporary(path) && *modified < stale {
                fs::remove_file(path).ok();
            }
        }
        if total <= self.budget {
            return Ok(());
        }
        entries.sort_by_key(|e| e.2);
        let target = self.budget / 100 * EVICT_TO_PERCENT;
        let mut remaining = total;
        for (path, size, _) in entries {
            if remaining <= target {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                remaining -= size;
            }
        }
        Ok(())
    }

    /// Starts [`Cache::evict`] on a background thread unless some process
    /// did within [`EVICT_INTERVAL`].
    fn maybe_evict(&self) {
        let stamp = self.dir.join("last-evict");
        let recent = fs::metadata(&stamp)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age < EVICT_INTERVAL));
        if recent || fs::write(&stamp, b"").is_err() {
            return;
        }
        let cache = self.clone();
        drop(
            std::thread::Builder::new()
                .name("dffy-cache-evict".into())
                .spawn(move || cache.evict()),
        );
    }
}

fn is_temporary(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.as_encoded_bytes().starts_with(b".tmp"))
}

/// Whether `bytes` is a whole, uncorrupted entry of `kind`.
fn intact(bytes: &[u8], kind: Kind) -> bool {
    let Some(header) = bytes.get(..HEADER) else {
        return false;
    };
    let field = |range: std::ops::Range<usize>| {
        u64::from_le_bytes(header[range].try_into().unwrap_or_default())
    };
    &header[..4] == MAGIC
        && header[4] == FORMAT
        && header[5] == kind as u8
        && (bytes.len() - HEADER) as u64 == field(6..14)
        && checksum(&bytes[HEADER..]) == field(14..22)
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut hasher = FxHasher::default();
    hasher.write(bytes);
    hasher.finish()
}

fn write_atomic(path: &Path, kind: Kind, value: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or(path);
    // Private: rendered entries hold file text.
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        let mut header = Vec::with_capacity(HEADER);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&[FORMAT, kind as u8]);
        header.extend_from_slice(&(value.len() as u64).to_le_bytes());
        header.extend_from_slice(&checksum(value).to_le_bytes());
        file.write_all(&header)?;
        file.write_all(value)?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        fs::remove_file(&tmp).ok();
    }
    result
}

/// SHA-1 of `bytes`, for building keys from large inputs.
#[must_use]
pub fn digest(bytes: &[u8]) -> ObjectId {
    let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
    hasher.update(bytes);
    hasher
        .try_finalize()
        .unwrap_or_else(|_| ObjectId::null(gix::hash::Kind::Sha1))
}

/// Appends `n` as a LEB128 varint.
pub(crate) fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let low = n.to_le_bytes()[0] & 0x7f;
        n >>= 7;
        if n == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

/// Reads a LEB128 varint from the front of `input`.
pub(crate) fn get_varint(input: &mut &[u8]) -> Option<u64> {
    let mut n = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = input.split_first()?;
        *input = rest;
        n |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(n);
        }
    }
    None
}

#[cfg(test)]
mod tests;

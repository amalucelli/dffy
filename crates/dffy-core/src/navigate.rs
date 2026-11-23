//! Go-to-definition without a language server: the definitions a name has
//! at one side of a diff, found by searching rather than resolving, and
//! ranked by how close each is to the file that asks.
//!
//! A [`Navigator`] is one side of a diff, and answers from three tiers:
//!
//! 1. The local tier: the asking file, the files it includes or uses, and
//!    the diff's files nearest it, read from the symbol cache or parsed on
//!    demand, briefly, when the index hasn't reached them yet.
//! 2. The index: every file of a language with a tags query, its
//!    definitions read from the symbol cache's packs or extracted and
//!    stored there. The host builds it a chunk at a time on its own
//!    threads ([`Navigator::take_chunk`], [`Chunk::run`],
//!    [`Navigator::add`]); a lookup answers from what's built so far and
//!    says how far that is.
//! 3. The text search ([`Navigator::search_text`]), for a name the index
//!    has no definition of: its whole-word uses, labelled as such.
//!
//! Names are matched, not resolved: a Rust method call, whose receiver's
//! type is unknown, gets every method of its name with the type each is on.
//!
//! The working tree has no tree naming its files: its side is the diff's
//! other snapshot with the diff's changes laid over it, and the changed
//! files' definitions are extracted from their text and kept in memory,
//! never cached under a blob id.

use std::cmp::Reverse;
use std::hash::{Hash as _, Hasher as _};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak, mpsc};
use std::time::Duration;

use gix::bstr::{BStr, BString, ByteSlice};
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

use crate::cache::SymbolCache;
use crate::diff::{LineKind, Options};
use crate::git::{self, FileChange, ObjectId, Repo, Side, Snapshot};
use crate::highlight::{
    self, DefinitionKind, Definitions, Language, Pack, Registry, Skipped, TagsError,
};
use crate::view;

mod imports;
mod names;
mod references;

pub use names::{name_at, names};
pub use references::{MAX_REFERENCE_FILES, MAX_REFERENCE_LINES, Reference, References};

/// How many of the asking file's included files, directly or through
/// others, count as included. Past it a kernel source's headers are mostly
/// the same core ones again.
const MAX_INCLUDED: usize = 256;

/// How many of the diff's files the local tier reads for a lookup, nearest
/// the asking file first; the index covers the rest.
const LOCAL_DIFF_FILES: usize = 32;

/// How long the local tier parses a file the index hasn't reached. One that
/// takes longer is left to the index build, which gives it longer.
const INLINE_PARSE: Duration = Duration::from_millis(100);

/// How long the local tier reads the files the index hasn't reached, in
/// all, for a lookup. What it leaves goes first in the index build.
const LOCAL_BUDGET: Duration = Duration::from_millis(100);

/// How many of a lookup's best ranked candidates have their file diffed, if
/// it changed, to say whether they're in a hunk.
const FACT_DIFFS: usize = 8;

/// How many candidates a lookup returns, best first.
const MAX_CANDIDATES: usize = 200;

/// Files the text search skips, by their size before they're read:
/// generated tables, not code a name is used in.
const MAX_SEARCH_BYTES: u64 = 8 << 20;

/// Files of a working-tree overlay per chunk of the index build.
const OVERLAY_CHUNK: usize = 64;

/// The architecture a file outside `arch/` includes the headers of, and
/// whose definitions rank first among the architectures'.
const DEFAULT_ARCH: &[u8] = b"x86";

/// The ids [`Navigator`]s take, unique in the process.
static NEXT_NAVIGATOR: AtomicU64 = AtomicU64::new(0);

/// Whether a navigator answers for a diff of its side read again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Nothing it answers from moved: keep it as it is.
    Same,
    /// Its files are as they were, but what differs between the diff's
    /// sides moved: it took the changes again.
    Changes,
    /// Its files moved: make another, from the same [`Shared`] store.
    New,
}

/// What a navigator of `diff` answers from, as one number: the diff's
/// snapshots and each change's paths, status and blob ids, and for a
/// working-tree file without an id its size and modification time, read
/// with one stat per such file. Two diffs with the same fingerprint have
/// the same navigators.
#[must_use]
pub fn fingerprint(repo: &Repo, diff: &Diff) -> u64 {
    let mut h = FxHasher::default();
    diff.snapshots.hash(&mut h);
    diff.changes.len().hash(&mut h);
    for change in diff.changes {
        change.path.hash(&mut h);
        change.old_path().hash(&mut h);
        std::mem::discriminant(&change.status).hash(&mut h);
        for side in [change.old, change.new] {
            match side {
                Some(Side::Worktree { id: None, .. }) => {
                    (repo.on_disk(change.path.as_bstr()))
                        .and_then(|path| stamp(&path))
                        .hash(&mut h);
                }
                side => side.and_then(|s| s.id()).hash(&mut h),
            }
        }
    }
    h.finish()
}

/// A file's size and modification time, which tell a rewrite of it apart
/// without reading it; `None` when it can't be read.
type Stamp = Option<(u64, Option<Duration>)>;

fn stamp(path: &std::path::Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    let modified =
        (meta.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    Some((meta.len(), modified))
}

/// The stamp of the files `snapshot` lists, when they can move: the index
/// file's for the index. A tree's never move; a working tree's are
/// stamped file by file in the [`fingerprint`].
fn listing_stamp(repo: &Repo, snapshot: Snapshot) -> Stamp {
    match snapshot {
        Snapshot::Index => stamp(&repo.git_dir()?.join("index")),
        _ => None,
    }
}

/// Everything that differs between `snapshots`, whatever paths a view
/// lists, oriented from the first to the second: the changes a
/// navigator's [`Diff`] takes. None between two working trees.
///
/// # Errors
///
/// When the repository can't be read.
pub fn changes_between(
    repo: &Repo,
    snapshots: [Snapshot; 2],
) -> Result<Vec<FileChange>, git::Error> {
    let mut changes = Vec::new();
    let Some((spec, reversed)) = git::DiffSpec::between(snapshots) else {
        return Ok(changes);
    };
    repo.for_each_change(&spec, &[], |change| {
        changes.push(if reversed { change.reversed() } else { change });
        std::ops::ControlFlow::Continue(())
    })?;
    Ok(changes)
}

/// Whether `change`'s two sides differ: not the same blob on both.
fn differs(change: &FileChange) -> bool {
    !matches!(
        (change.old.and_then(|s| s.id()), change.new.and_then(|s| s.id())),
        (Some(old), Some(new)) if old == new
    )
}

/// The diff a [`Navigator`] is a side of.
#[derive(Debug, Clone, Copy)]
pub struct Diff<'a> {
    /// Its two sides, from [`git::DiffSpec::snapshots`].
    pub snapshots: [Snapshot; 2],
    /// Everything that differs between them, as
    /// [`Repo::for_each_change`] lists it with no path filter, renames
    /// paired or not: the working tree's side is the other one with these
    /// laid over it, and a file missing from them is taken as unchanged.
    pub changes: &'a [FileChange],
    /// The options the host diffs with, for what counts as a hunk.
    pub options: Options,
}

/// Definitions read once and shared by every navigator a host makes with
/// it, both sides of a diff and those made again after a reload, by blob.
/// It holds them weakly: a blob's stay as long as a navigator has them.
/// Cloning shares it.
#[derive(Debug, Clone, Default)]
pub struct Shared(Arc<Mutex<SharedEntries>>);

#[derive(Debug, Default)]
struct SharedEntries {
    by_key: FxHashMap<ObjectId, Weak<Entry>>,
    /// The size past which entries no navigator holds are dropped.
    prune_at: usize,
}

impl Shared {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn entries(&self) -> MutexGuard<'_, SharedEntries> {
        // Nothing panics while the map is held half changed.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self, key: &ObjectId) -> Option<Arc<Entry>> {
        self.entries().by_key.get(key)?.upgrade()
    }

    fn insert(&self, key: ObjectId, entry: &Arc<Entry>) {
        let mut shared = self.entries();
        shared.by_key.insert(key, Arc::downgrade(entry));
        if shared.by_key.len() > shared.prune_at {
            shared.by_key.retain(|_, e| e.strong_count() > 0);
            shared.prune_at = (2 * shared.by_key.len()).max(1024);
        }
    }
}

/// The file a lookup is asked from.
#[derive(Debug, Clone)]
pub struct Origin {
    /// Repository-relative, at the navigator's side.
    pub path: BString,
    /// Its language: candidates are definitions in files of the same one,
    /// or of one whose files it imports (TypeScript, TSX and JavaScript;
    /// C headers for C++). With an empty path, no file asks, and only the
    /// language's own files answer.
    pub language: Language,
}

/// How close a candidate is to the asking file, closest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rank {
    /// In the asking file.
    SameFile,
    /// In a file the asking file includes, directly or through another (C
    /// and C++), or imports (a Rust `use`, any other language's import),
    /// or that such a file re-exports from.
    Imported,
    /// In the asking file's folder, or for Rust its crate.
    Near,
    /// Anywhere else.
    Elsewhere,
}

/// One answer to a lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Repository-relative.
    pub path: BString,
    /// Where the file is at the navigator's side: a blob, or the
    /// working-tree file.
    pub side: Side,
    /// What it defines; `None` for a text match, a use of the name that
    /// may not define it.
    pub kind: Option<DefinitionKind>,
    /// Its first line, 0-based.
    pub start: usize,
    /// Its last line, 0-based; a text match's line.
    pub end: usize,
    /// For a member, the type it's in (see
    /// [`highlight::Definition::container`]).
    pub container: Option<String>,
    /// How close it is to the asking file.
    pub rank: Rank,
    /// The file differs between the diff's two sides.
    pub changed: bool,
    /// Its lines hold a line the diff added or removed on this side, or the
    /// place lines were removed from. `None` when not worked out: only the
    /// best ranked candidates' files are diffed, and one whose diff fails
    /// stays unknown.
    pub in_hunk: Option<bool>,
}

/// How far the index is built, in files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    /// Files whose definitions are in, or that have none to give.
    pub done: usize,
    /// Files of languages with a tags query at the side.
    pub total: usize,
}

impl Progress {
    /// Every file is in.
    #[must_use]
    pub fn complete(self) -> bool {
        self.done >= self.total
    }
}

/// What the index build did, summed over its chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BuildStats {
    /// Files whose definitions were already read: by a navigator sharing
    /// them, or into the symbol cache.
    pub cached: usize,
    /// Files parsed for their definitions.
    pub extracted: usize,
    /// Files larger than [`highlight::MAX_TAGS_BYTES`], with none.
    pub too_large: usize,
    /// Files whose parse took too long, with none for now.
    pub timed_out: usize,
    /// Files that couldn't be read.
    pub unreadable: usize,
}

impl BuildStats {
    fn add(&mut self, other: Self) {
        self.cached += other.cached;
        self.extracted += other.extracted;
        self.too_large += other.too_large;
        self.timed_out += other.timed_out;
        self.unreadable += other.unreadable;
    }
}

/// A lookup's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lookup {
    /// The definitions of the name found, best first, up to 200. One or
    /// many, the host decides what to show.
    pub candidates: Vec<Candidate>,
    /// How many were found, beyond the ones listed.
    pub total: usize,
    /// How far the index was built when it answered: incomplete means a
    /// definition may be missing.
    pub progress: Progress,
}

/// A text search's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSearch {
    /// Each line using the name as a whole word, as candidates without a
    /// kind, the closest files first.
    pub matches: Vec<Candidate>,
    /// Every file was searched: not stopped at the cap or cancelled.
    pub complete: bool,
}

/// A text search a host steps, from [`Navigator::text_matches`]: the lines
/// using a name as a whole word, closest files first, up to its cap.
#[derive(Debug)]
pub struct TextMatches {
    /// The navigator it searches, the only one it steps with.
    navigator: u64,
    name: String,
    finder: memchr::memmem::Finder<'static>,
    /// The side's files, closest first, and the next to search.
    order: Vec<(Rank, u32)>,
    next: usize,
    /// Lines it may still find, and those found.
    left: usize,
    found: usize,
}

impl TextMatches {
    /// The name searched for.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Every file is searched, or the cap is reached: nothing more comes.
    #[must_use]
    pub fn done(&self) -> bool {
        self.next >= self.order.len() || self.left == 0
    }

    /// How many of the side's files are searched.
    #[must_use]
    pub fn searched(&self) -> Progress {
        Progress {
            done: self.next,
            total: self.order.len(),
        }
    }

    /// Searches the next files of `navigator`, the one this search came
    /// from, until it's done or `stop` says so, asked before each file.
    /// Returns the lines found, what their files' changes say included.
    /// With another navigator it finds nothing, and is done.
    pub fn step(
        &mut self,
        navigator: &Navigator,
        repo: &Repo,
        mut stop: impl FnMut() -> bool,
    ) -> Vec<Candidate> {
        if navigator.id != self.navigator {
            self.next = self.order.len();
            return Vec::new();
        }
        let word = self.name.as_bytes();
        let mut out = Vec::new();
        let mut stopped = false;
        while !self.done() {
            if stop() {
                stopped = true;
                break;
            }
            let (rank, l) = self.order[self.next];
            self.next += 1;
            let listed = &navigator.listed[l as usize];
            let (path, side) = (listed.path.as_ref(), &listed.side);
            if repo
                .size(path, side)
                .is_ok_and(|size| size > MAX_SEARCH_BYTES)
            {
                continue;
            }
            let Ok(text) = repo.load(path, side) else {
                continue;
            };
            if git::is_binary(&text) {
                continue;
            }
            let (mut line, mut counted, mut last) = (0, 0, None);
            for at in self.finder.find_iter(&text) {
                let end = at + word.len();
                if (at > 0 && is_word_byte(text[at - 1]))
                    || text.get(end).is_some_and(|&b| is_word_byte(b))
                {
                    continue;
                }
                line += memchr::memchr_iter(b'\n', &text[counted..at]).count();
                counted = at;
                if last == Some(line) {
                    continue;
                }
                last = Some(line);
                out.push(Candidate {
                    path: listed.path.clone(),
                    side: listed.side,
                    kind: None,
                    start: line,
                    end: line,
                    container: None,
                    rank,
                    changed: false,
                    in_hunk: None,
                });
                self.left -= 1;
                if self.left == 0 {
                    break;
                }
            }
        }
        // Stopped, it doesn't diff files for the facts either.
        navigator.facts_from(repo, &mut out, self.found, &AtomicBool::new(stopped));
        self.found += out.len();
        out
    }
}

/// Why a lookup failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading the repository failed.
    #[error(transparent)]
    Git(#[from] git::Error),
    /// The asking file's language has a tags query that doesn't compile.
    #[error(transparent)]
    Tags(#[from] TagsError),
    /// The host cancelled it.
    #[error("cancelled")]
    Cancelled,
}

/// A file at the side.
#[derive(Debug)]
struct Listed {
    path: BString,
    side: Side,
    /// Its number among the indexed files, if it's of a language with a
    /// tags query.
    indexed: Option<u32>,
}

/// A file of a language with a tags query.
#[derive(Debug)]
struct Indexed {
    listed: u32,
    language: u8,
    /// Its [`highlight::pack_key`]; `None` for a working-tree file, which
    /// is never cached.
    key: Option<ObjectId>,
}

/// A file's definitions, kept compact: the cached form, and the hashes of
/// the names in it to tell without decoding whether it has a name.
#[derive(Debug)]
struct Entry {
    encoded: Box<[u8]>,
    hashes: Box<[u32]>,
}

impl Entry {
    fn new(defs: &Definitions, encoded: Box<[u8]>) -> Self {
        let mut hashes: Vec<u32> = defs.names().map(hash).collect();
        hashes.sort_unstable();
        hashes.dedup();
        Self {
            encoded,
            hashes: hashes.into(),
        }
    }

    fn has(&self, hash: u32) -> bool {
        self.hashes.binary_search(&hash).is_ok()
    }
}

/// The [`hash`] of each word of letters, digits and `_` in `text`.
fn words(text: &[u8]) -> FxHashSet<u32> {
    (text.split(|&b| !is_word_byte(b)))
        .filter(|w| !w.is_empty())
        .filter_map(|w| std::str::from_utf8(w).ok())
        .map(hash)
        .collect()
}

fn hash(name: &str) -> u32 {
    let mut hasher = FxHasher::default();
    hasher.write(name.as_bytes());
    let [a, b, c, d, ..] = hasher.finish().to_le_bytes();
    u32::from_le_bytes([a, b, c, d])
}

#[derive(Debug)]
enum Slot {
    Todo,
    /// In a chunk being run.
    Taken,
    /// `None` for a file with no definitions to give: too large, timed
    /// out, unreadable.
    Found(Option<Arc<Entry>>),
}

/// The files the index hasn't got yet, a chunk each: one of the symbol
/// cache's packs, or a run of working-tree files.
#[derive(Debug)]
struct Queued {
    pack: Option<usize>,
    files: Vec<u32>,
}

#[derive(Debug)]
struct State {
    slots: Vec<Slot>,
    /// Run from the end.
    queue: Vec<Queued>,
    done: usize,
    stats: BuildStats,
    related: FxHashMap<BString, Arc<Related>>,
    /// Each name's [`hash`] with each indexed file whose definitions have
    /// it, sorted, so a lookup reads only those files; and those taken in
    /// since it was last sorted.
    postings: Vec<(u32, u32)>,
    fresh: Vec<(u32, u32)>,
    /// Files whose parse took longer than the local tier gives one: later
    /// lookups leave them to the index build.
    timed_out: FxHashSet<u32>,
    /// Files put first in the build queue for a lookup.
    front: FxHashSet<u32>,
}

impl State {
    /// The indexed files whose definitions have a name hashed `wanted`,
    /// sorting in what was taken in since the last ask.
    fn posted(&mut self, wanted: u32) -> impl Iterator<Item = u32> + '_ {
        if !self.fresh.is_empty() {
            let mut fresh = std::mem::take(&mut self.fresh);
            fresh.sort_unstable();
            let old = std::mem::take(&mut self.postings);
            let mut merged = Vec::with_capacity(old.len() + fresh.len());
            let (mut i, mut j) = (0, 0);
            while i < old.len() && j < fresh.len() {
                if old[i] <= fresh[j] {
                    merged.push(old[i]);
                    i += 1;
                } else {
                    merged.push(fresh[j]);
                    j += 1;
                }
            }
            merged.extend_from_slice(&old[i..]);
            merged.extend_from_slice(&fresh[j..]);
            self.postings = merged;
        }
        let start = self.postings.partition_point(|p| p.0 < wanted);
        let end = self.postings.partition_point(|p| p.0 <= wanted);
        self.postings[start..end].iter().map(|p| p.1)
    }
}

/// The files close to an asking file: itself at depth 0, then those it
/// includes or uses, by how many steps away; and the [`hash`]es of the
/// words it holds, which a candidate's container is looked for among.
#[derive(Debug, Default)]
struct Related {
    files: Vec<(u32, u8)>,
    words: FxHashSet<u32>,
}

/// The lines a diff touches on one side of a file.
#[derive(Debug, Default)]
struct Touched {
    /// Lines of the side added or removed, 0-based, sorted.
    lines: Vec<usize>,
    /// Where lines of the other side were, as the line after them.
    between: Vec<usize>,
}

impl Touched {
    fn hits(&self, start: usize, end: usize) -> bool {
        let line = self.lines.partition_point(|&l| l < start);
        let gap = self.between.partition_point(|&p| p <= start);
        self.lines.get(line).is_some_and(|&l| l <= end)
            || self.between.get(gap).is_some_and(|&p| p <= end)
    }
}

/// One side of a diff, for looking up where names are defined.
///
/// Every method takes `&self`, so a host can share it between the thread
/// that looks names up and the ones building its index.
#[derive(Debug)]
pub struct Navigator {
    /// Unique among the process's navigators, for what's tied to one.
    id: u64,
    side: view::Side,
    /// The diff's snapshots it was made for, and the stamp of its side's
    /// listing.
    snapshots: [Snapshot; 2],
    listing: Stamp,
    /// By path.
    listed: Vec<Listed>,
    indexed: Vec<Indexed>,
    languages: Vec<Language>,
    /// The diff's changes, by their path on this side, and their
    /// [`fingerprint`].
    changes: Mutex<(Arc<FxHashMap<BString, FileChange>>, u64)>,
    /// The indexed files among the changes, as of the last refit.
    diffed: Mutex<Arc<Vec<u32>>>,
    options: Options,
    /// `arch/<name>/include/` for each architecture with headers.
    arches: Vec<BString>,
    /// The folders holding a `Cargo.toml`, with a trailing `/`; empty for
    /// the root.
    crates: Vec<BString>,
    shared: Shared,
    state: Mutex<State>,
    touched: Mutex<FxHashMap<BString, Arc<Touched>>>,
    /// [`LOCAL_BUDGET`], but in tests.
    local_budget: Duration,
}

impl Navigator {
    /// The side `side` of `diff`, sharing definitions through `shared`.
    /// Lists the side's files, and for the working tree the other
    /// snapshot's with the changes laid over it; no file is read.
    ///
    /// # Errors
    ///
    /// When the snapshot can't be listed.
    pub fn new(
        repo: &Repo,
        grammars: &Registry,
        shared: &Shared,
        diff: &Diff,
        side: view::Side,
    ) -> Result<Self, git::Error> {
        // Stamped before listing: a file moving meanwhile shows as moved.
        let (print, listing) = (
            fingerprint(repo, diff),
            listing_stamp(repo, diff.snapshots[side.index()]),
        );
        let files = side_files(repo, diff.snapshots, side, diff.changes)?;
        let mut languages: Vec<Language> = Vec::new();
        let mut listed = Vec::with_capacity(files.len());
        let mut indexed = Vec::new();
        let (mut arches, mut crates) = (Vec::new(), Vec::new());
        for (path, file) in files {
            if let Some(dir) = path.strip_suffix(b"Cargo.toml")
                && (dir.is_empty() || dir.ends_with(b"/"))
            {
                crates.push(BString::from(dir));
            }
            if let Some(rest) = path.strip_prefix(b"arch/")
                && let Some((arch, rest)) = rest.split_once_str("/")
                && rest.starts_with(b"include/")
            {
                arches.push(BString::from(format!("arch/{}/include/", arch.as_bstr())));
            }
            let language = grammars.detect(&path, b"").filter(|l| l.has_tags());
            let number = language.map(|language| {
                let n = languages
                    .iter()
                    .position(|l| l.name() == language.name())
                    .unwrap_or_else(|| {
                        languages.push(language.clone());
                        languages.len() - 1
                    });
                let key = match file {
                    Side::Object { id, .. } => Some(highlight::pack_key(language, id)),
                    Side::Worktree { .. } | Side::Path { .. } => None,
                };
                indexed.push(Indexed {
                    listed: to_u32(listed.len()),
                    language: u8::try_from(n).unwrap_or(u8::MAX),
                    key,
                });
                to_u32(indexed.len() - 1)
            });
            listed.push(Listed {
                path,
                side: file,
                indexed: number,
            });
        }
        arches.dedup();
        let state = State {
            slots: (0..indexed.len()).map(|_| Slot::Todo).collect(),
            queue: queue(&indexed),
            done: 0,
            stats: BuildStats::default(),
            related: FxHashMap::default(),
            postings: Vec::new(),
            fresh: Vec::new(),
            timed_out: FxHashSet::default(),
            front: FxHashSet::default(),
        };
        let changes = by_path(diff.changes, side);
        let diffed = diffed(&listed, &changes);
        Ok(Self {
            id: NEXT_NAVIGATOR.fetch_add(1, Ordering::Relaxed),
            side,
            snapshots: diff.snapshots,
            listing,
            changes: Mutex::new((Arc::new(changes), print)),
            diffed: Mutex::new(Arc::new(diffed)),
            options: diff.options,
            listed,
            indexed,
            languages,
            arches,
            crates,
            shared: shared.clone(),
            state: Mutex::new(state),
            touched: Mutex::new(FxHashMap::default()),
            local_budget: LOCAL_BUDGET,
        })
    }

    /// This navigator with the local tier given `budget` in all.
    #[cfg(test)]
    fn with_local_budget(mut self, budget: Duration) -> Self {
        self.local_budget = budget;
        self
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Nothing panics while the state is held half changed.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn changes(&self) -> Arc<FxHashMap<BString, FileChange>> {
        Arc::clone(
            &self
                .changes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .0,
        )
    }

    /// Brings this navigator up to `diff`, a diff of the same side read
    /// again, when it can: kept as it is, or taking the diff's changes for
    /// what a candidate says about changing in the range and for the local
    /// tier's nearest files, its files kept as listed. [`Fit::New`] when its
    /// files moved, for the host to make another. The one decision hosts
    /// make about keeping a navigator.
    pub fn refit(&self, repo: &Repo, diff: &Diff) -> Fit {
        let print = fingerprint(repo, diff);
        let at = self.side.index();
        let mut changes = self.changes.lock().unwrap_or_else(PoisonError::into_inner);
        if print == changes.1 {
            return Fit::Same;
        }
        let unmoved = diff.snapshots[at] == self.snapshots[at]
            && match self.snapshots[at] {
                Snapshot::Tree(_) => true,
                Snapshot::Index => listing_stamp(repo, Snapshot::Index) == self.listing,
                Snapshot::Worktree => false,
            };
        if !unmoved {
            return Fit::New;
        }
        let moved = by_path(diff.changes, self.side);
        *self.diffed.lock().unwrap_or_else(PoisonError::into_inner) =
            Arc::new(diffed(&self.listed, &moved));
        *changes = (Arc::new(moved), print);
        drop(changes);
        self.touched().clear();
        Fit::Changes
    }

    /// Unique among the process's navigators: what's tied to this one
    /// tells it apart from one made again for the same diff.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// What it answers from, as [`fingerprint`] has it for the diff it was
    /// last brought up to.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        self.changes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .1
    }

    /// How far the index is built.
    #[must_use]
    pub fn progress(&self) -> Progress {
        Progress {
            done: self.state().done,
            total: self.indexed.len(),
        }
    }

    /// The languages of the side's files with a tags query: those a lookup
    /// can be asked from.
    #[must_use]
    pub fn languages(&self) -> &[Language] {
        &self.languages
    }

    /// What the index build has done so far.
    #[must_use]
    pub fn stats(&self) -> BuildStats {
        self.state().stats
    }

    /// The next chunk of the index to build, taken from the queue: run it
    /// with [`Chunk::run`] on any thread and hand the result to
    /// [`Navigator::add`], or hand it back unrun with
    /// [`Navigator::give_back`]. `None` when nothing is left to take. A
    /// chunk taken and never returned stays out of the build.
    #[must_use]
    pub fn take_chunk(&self) -> Option<Chunk> {
        let mut state = self.state();
        while let Some(queued) = state.queue.pop() {
            let mut work = Vec::new();
            for f in queued.files {
                let slot = &mut state.slots[f as usize];
                if matches!(slot, Slot::Todo) {
                    *slot = Slot::Taken;
                    work.push(self.work(f));
                }
            }
            if !work.is_empty() {
                return Some(Chunk {
                    pack: queued.pack,
                    work,
                    shared: self.shared.clone(),
                });
            }
        }
        None
    }

    fn work(&self, file: u32) -> Work {
        let indexed = &self.indexed[file as usize];
        let listed = &self.listed[indexed.listed as usize];
        Work {
            file,
            path: listed.path.clone(),
            side: listed.side,
            key: indexed.key,
            language: self.languages[usize::from(indexed.language)].clone(),
        }
    }

    /// Takes in a chunk's result. Files a cancelled chunk didn't reach go
    /// back in the queue, to be taken first. The error is a language whose
    /// tags query doesn't compile, for the host to show; its files have no
    /// definitions.
    #[must_use]
    pub fn add(&self, done: Done) -> Option<TagsError> {
        self.take_in(done.found, done.stats);
        self.requeue(done.pack, done.rest);
        done.error
    }

    /// Puts an unrun chunk back in the queue, to be taken first.
    pub fn give_back(&self, chunk: Chunk) {
        self.requeue(chunk.pack, chunk.work.into_iter().map(|w| w.file).collect());
    }

    fn take_in(&self, found: Vec<(u32, Option<Arc<Entry>>)>, did: BuildStats) {
        let mut state = self.state();
        for (f, entry) in found {
            if matches!(state.slots[f as usize], Slot::Found(_)) {
                continue;
            }
            if let Some(entry) = &entry {
                state.fresh.extend(entry.hashes.iter().map(|&h| (h, f)));
            }
            state.slots[f as usize] = Slot::Found(entry);
            state.done += 1;
        }
        state.stats.add(did);
    }

    /// Puts those of `files` (indexed numbers) not built yet first in the
    /// build queue, each with its pack's, once per navigator.
    fn prioritize(&self, files: impl IntoIterator<Item = u32>) {
        let mut state = self.state();
        let mut by_pack: FxHashMap<Option<usize>, Vec<u32>> = FxHashMap::default();
        for f in files {
            if matches!(state.slots[f as usize], Slot::Todo) && state.front.insert(f) {
                let pack = self.indexed[f as usize]
                    .key
                    .as_ref()
                    .map(highlight::pack_of);
                by_pack.entry(pack).or_default().push(f);
            }
        }
        let mut by_pack: Vec<_> = by_pack.into_iter().collect();
        by_pack.sort_unstable();
        // The queue runs from its end.
        state.queue.extend(
            by_pack
                .into_iter()
                .rev()
                .map(|(pack, files)| Queued { pack, files }),
        );
    }

    fn requeue(&self, pack: Option<usize>, files: Vec<u32>) {
        let mut state = self.state();
        let files: Vec<u32> = (files.into_iter())
            .filter(|&f| {
                let slot = &mut state.slots[f as usize];
                let taken = matches!(slot, Slot::Taken);
                if taken {
                    *slot = Slot::Todo;
                }
                taken
            })
            .collect();
        if !files.is_empty() {
            state.queue.push(Queued { pack, files });
        }
    }

    /// Builds the rest of the index on `threads` threads of its own,
    /// calling `progress` on this one after each chunk, until it's built
    /// or `cancel` is set. Cancelled, it resumes where it stopped on the
    /// next call.
    ///
    /// # Errors
    ///
    /// The first language found whose tags query doesn't compile; the
    /// build goes on without it.
    pub fn build(
        &self,
        repo: &Repo,
        cache: &SymbolCache,
        threads: usize,
        cancel: &AtomicBool,
        mut progress: impl FnMut(Progress),
    ) -> Result<(), TagsError> {
        let (tx, rx) = mpsc::channel();
        let mut error = None;
        std::thread::scope(|s| {
            for _ in 0..threads.max(1) {
                let (tx, repo) = (tx.clone(), repo.without_object_cache());
                s.spawn(move || {
                    while !cancel.load(Ordering::Relaxed)
                        && let Some(chunk) = self.take_chunk()
                    {
                        let failed = self.add(chunk.run(&repo, cache, cancel));
                        if tx.send(failed).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(tx);
            for failed in rx {
                if error.is_none() {
                    error = failed;
                }
                progress(self.progress());
            }
        });
        error.map_or(Ok(()), Err)
    }

    fn find(&self, path: &[u8]) -> Option<u32> {
        (self.listed)
            .binary_search_by(|l| l.path.as_slice().cmp(path))
            .ok()
            .map(to_u32)
    }

    /// Fills the entries of `files` (indexed numbers) not built yet, in
    /// their order: from the shared store or the symbol cache, a pack read
    /// once and written once for all of its files, or by a parse. The first
    /// file is the asking one: it's always read, with the build's parse
    /// budget, so its own definitions never wait for the build. The rest
    /// get [`INLINE_PARSE`] each within [`LOCAL_BUDGET`] in all; one whose
    /// parse takes longer, and those left when the time is up, stay for the
    /// index build, first in its queue, and one that took too long isn't
    /// parsed here again.
    fn fill(
        &self,
        repo: &Repo,
        cache: &SymbolCache,
        files: &[u32],
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        let deadline = std::time::Instant::now() + self.local_budget;
        let todo: Vec<u32> = {
            let state = self.state();
            let mut seen = FxHashSet::default();
            (files.iter().enumerate())
                .filter(|&(i, &f)| {
                    seen.insert(f)
                        && !matches!(state.slots[f as usize], Slot::Found(_))
                        && (i == 0 || !state.timed_out.contains(&f))
                })
                .map(|(_, &f)| f)
                .collect()
        };
        let asking = files.first().copied();
        let mut packs: FxHashMap<usize, LazyPack> = FxHashMap::default();
        let (mut found, mut stats, mut error) = (Vec::new(), BuildStats::default(), None);
        let (mut cancelled, mut left) = (false, Vec::new());
        for f in todo {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            let budget = if Some(f) == asking {
                highlight::TAGS_PARSE_BUDGET
            } else {
                let budget = deadline.saturating_duration_since(std::time::Instant::now());
                if budget.is_zero() {
                    left.push(f);
                    continue;
                }
                budget.min(INLINE_PARSE)
            };
            let pack = (self.indexed[f as usize].key.as_ref())
                .map(highlight::pack_of)
                .map(|p| packs.entry(p).or_insert_with(|| LazyPack::new(cache, p)));
            match self
                .work(f)
                .entry(repo, pack, &self.shared, budget, &mut stats)
            {
                Ok(Got::Entry(entry)) => found.push((f, entry)),
                Ok(Got::TimedOut) => {
                    self.state().timed_out.insert(f);
                    left.push(f);
                }
                Err(e) => {
                    error = Some(e);
                    found.push((f, None));
                }
            }
        }
        for pack in packs.values_mut() {
            pack.write();
        }
        self.take_in(found, stats);
        self.prioritize(left);
        if cancelled {
            return Err(Error::Cancelled);
        }
        error.map_or(Ok(()), |e| Err(e.into()))
    }

    /// Where `name` is defined, as seen from `origin`: the definitions in
    /// files of the origin's language (see [`Origin::language`]) that the
    /// index has so far, and in the local tier's files, which it reads now
    /// if the index hasn't. Re-exports come after every definition, and a
    /// Python module's relative imports count as re-exports only in an
    /// `__init__.py`. Ranked by [`Rank`], then a member of a type the
    /// origin names before others, then by how far an included file is, then
    /// architectures other than the origin's (or x86 outside `arch/`)
    /// after the rest, then by how many leading folders a file shares with
    /// the origin, then by path and line. It stops between files once
    /// `cancel` is set.
    ///
    /// # Errors
    ///
    /// [`Error::Cancelled`] when cancelled; when the origin's language's
    /// tags query doesn't compile, or the repository can't be read.
    pub fn lookup(
        &self,
        repo: &Repo,
        cache: &SymbolCache,
        name: &str,
        origin: &Origin,
        cancel: &AtomicBool,
    ) -> Result<Lookup, Error> {
        if name.is_empty() {
            return Ok(Lookup {
                candidates: Vec::new(),
                total: 0,
                progress: self.progress(),
            });
        }
        let related = self.related(repo, origin, cancel)?;
        let language = origin.language.name();
        // With no asking file, a host asks once per language: each answers
        // for its own files only.
        let sees = |of: &str| {
            if origin.path.is_empty() {
                of == language
            } else {
                imports::shares_definitions(language, of)
            }
        };
        let mut local: Vec<u32> = (related.files.iter())
            .filter(|(_, depth)| *depth <= 1)
            .filter_map(|&(l, _)| self.listed[l as usize].indexed)
            .collect();
        let diffed = Arc::clone(&self.diffed.lock().unwrap_or_else(PoisonError::into_inner));
        let mut changed: Vec<(Reverse<usize>, u32)> = (diffed.iter())
            .filter(|&&f| {
                let of = &self.languages[usize::from(self.indexed[f as usize].language)];
                sees(of.name())
            })
            .map(|&f| {
                let path = &self.listed[self.indexed[f as usize].listed as usize].path;
                (Reverse(shared_folders(path, &origin.path)), f)
            })
            .collect();
        if changed.len() > LOCAL_DIFF_FILES {
            changed.select_nth_unstable(LOCAL_DIFF_FILES);
            changed.truncate(LOCAL_DIFF_FILES);
        }
        changed.sort_unstable();
        local.extend(changed.iter().map(|&(_, f)| f));
        self.fill(repo, cache, &local, cancel)?;
        self.prioritize(
            related
                .files
                .iter()
                .filter_map(|&(l, _)| self.listed[l as usize].indexed),
        );

        let wanted = hash(name);
        let (hits, progress) = {
            let mut state = self.state();
            let posted: Vec<u32> = state.posted(wanted).collect();
            let hits: Vec<(u32, Arc<Entry>)> = (posted.into_iter())
                .filter_map(|f| match &state.slots[f as usize] {
                    Slot::Found(Some(entry)) => {
                        let indexed = &self.indexed[f as usize];
                        let of = self.languages[usize::from(indexed.language)].name();
                        sees(of).then(|| (indexed.listed, Arc::clone(entry)))
                    }
                    _ => None,
                })
                .collect();
            let progress = Progress {
                done: state.done,
                total: self.indexed.len(),
            };
            (hits, progress)
        };
        let mut candidates = self.defined(hits, name, language);
        self.rank(&mut candidates, &related, origin);
        let total = candidates.len();
        candidates.truncate(MAX_CANDIDATES);
        self.facts(repo, &mut candidates, cancel);
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        Ok(Lookup {
            candidates,
            total,
            progress,
        })
    }

    /// The definitions of `name` in `hits`, the indexed files that have
    /// it, with a re-export only where it is one, and the files that are
    /// module `name` in `language`; unranked.
    fn defined(&self, hits: Vec<(u32, Arc<Entry>)>, name: &str, language: &str) -> Vec<Candidate> {
        let candidate = |l: u32, kind, start, end, container| {
            let listed = &self.listed[l as usize];
            Candidate {
                path: listed.path.clone(),
                side: listed.side,
                kind: Some(kind),
                start,
                end,
                container,
                rank: Rank::Elsewhere,
                changed: false,
                in_hunk: None,
            }
        };
        let mut candidates = Vec::new();
        for (l, entry) in hits {
            let Some(defs) = Definitions::decode(&entry.encoded) else {
                continue;
            };
            let path = self.listed[l as usize].path.as_ref();
            for d in defs.iter().filter(|d| d.name == name) {
                if d.kind != DefinitionKind::Reexport || imports::reexports(path) {
                    let container = d.container.map(str::to_owned);
                    candidates.push(candidate(l, d.kind, d.start, d.end, container));
                }
            }
        }
        for l in self.module_files(name, language) {
            candidates.push(candidate(l, DefinitionKind::Module, 0, 0, None));
        }
        candidates
    }

    /// Each line of the side's files that uses `name` as a whole word, the
    /// files closest to `origin` first, up to `cap` lines or until
    /// `cancel` is set. Every file is searched, whatever its language;
    /// binary files, those over 8MB and those that can't be read are
    /// skipped.
    ///
    /// # Errors
    ///
    /// When the repository can't be read.
    pub fn search_text(
        &self,
        repo: &Repo,
        name: &str,
        origin: &Origin,
        cap: usize,
        cancel: &AtomicBool,
    ) -> Result<TextSearch, Error> {
        let mut search = match self.text_matches(repo, name, origin, cap, cancel) {
            Ok(search) => search,
            Err(Error::Cancelled) => {
                return Ok(TextSearch {
                    matches: Vec::new(),
                    complete: false,
                });
            }
            Err(e) => return Err(e),
        };
        let matches = search.step(self, repo, || cancel.load(Ordering::Relaxed));
        Ok(TextSearch {
            complete: search.done() && matches.len() < cap,
            matches,
        })
    }

    /// [`Navigator::search_text`] as a search a host steps a few files at
    /// a time, to stop and go on later: up to `cap` lines in all.
    ///
    /// # Errors
    ///
    /// [`Error::Cancelled`] when cancelled while reading what `origin`
    /// includes; when the repository can't be read.
    pub fn text_matches(
        &self,
        repo: &Repo,
        name: &str,
        origin: &Origin,
        cap: usize,
        cancel: &AtomicBool,
    ) -> Result<TextMatches, Error> {
        let order = if name.is_empty() || cap == 0 {
            Vec::new()
        } else {
            let related = self.related(repo, origin, cancel)?;
            self.search_order(&related, origin)
        };
        Ok(TextMatches {
            navigator: self.id,
            name: name.to_owned(),
            finder: memchr::memmem::Finder::new(name.as_bytes()).into_owned(),
            order,
            next: 0,
            left: cap,
            found: 0,
        })
    }

    /// Every file of the side, closest to `origin` first, as [`Navigator::lookup`]
    /// ranks candidates, with its rank.
    fn search_order(&self, related: &Related, origin: &Origin) -> Vec<(Rank, u32)> {
        let depths: FxHashMap<u32, u8> = related.files.iter().copied().collect();
        let arch = preferred_arch(origin.path.as_ref());
        let mut order: Vec<(Rank, u8, bool, Reverse<usize>, u32)> = (0..self.listed.len())
            .map(|l| {
                let l = to_u32(l);
                let path = &self.listed[l as usize].path;
                let depth = depths.get(&l).copied();
                (
                    self.rank_of(path.as_ref(), depth, origin),
                    depth.unwrap_or(u8::MAX),
                    foreign_arch(path.as_ref(), arch),
                    Reverse(shared_folders(path, &origin.path)),
                    l,
                )
            })
            .collect();
        order.sort_unstable();
        order.into_iter().map(|(rank, .., l)| (rank, l)).collect()
    }

    fn rank_of(&self, path: &BStr, depth: Option<u8>, origin: &Origin) -> Rank {
        match depth {
            Some(0) => Rank::SameFile,
            Some(_) => Rank::Imported,
            None if folder(path) == folder(origin.path.as_ref())
                || (origin.language.name() == "rust"
                    && self.crate_of(path).is_some()
                    && self.crate_of(path) == self.crate_of(origin.path.as_ref())) =>
            {
                Rank::Near
            }
            None => Rank::Elsewhere,
        }
    }

    fn rank(&self, candidates: &mut [Candidate], related: &Related, origin: &Origin) {
        let depths: FxHashMap<&BStr, u8> = (related.files.iter())
            .map(|&(l, depth)| (self.listed[l as usize].path.as_ref(), depth))
            .collect();
        let arch = preferred_arch(origin.path.as_ref());
        for c in candidates.iter_mut() {
            c.rank = self.rank_of(
                c.path.as_ref(),
                depths.get(c.path.as_bstr()).copied(),
                origin,
            );
        }
        candidates.sort_by_cached_key(|c| {
            let unmentioned = (c.container.as_deref())
                .is_some_and(|container| !related.words.contains(&hash(container)));
            (
                c.kind == Some(DefinitionKind::Reexport),
                c.rank,
                unmentioned,
                depths.get(c.path.as_bstr()).copied().unwrap_or(u8::MAX),
                foreign_arch(c.path.as_ref(), arch),
                Reverse(shared_folders(&c.path, &origin.path)),
                c.path.clone(),
                c.start,
            )
        });
    }

    /// Fills each candidate's [`Candidate::changed`] and, for the first
    /// [`FACT_DIFFS`] candidates and any whose file was diffed before,
    /// [`Candidate::in_hunk`]. Stops diffing once `cancel` is set.
    fn facts(&self, repo: &Repo, candidates: &mut [Candidate], cancel: &AtomicBool) {
        self.facts_from(repo, candidates, 0, cancel);
    }

    /// [`Navigator::facts`] of `candidates`, which come after `before`
    /// others the facts were worked out for.
    fn facts_from(
        &self,
        repo: &Repo,
        candidates: &mut [Candidate],
        before: usize,
        cancel: &AtomicBool,
    ) {
        let changes = self.changes();
        for (i, c) in (before..).zip(candidates.iter_mut()) {
            let Some(change) = changes.get(&c.path) else {
                c.in_hunk = Some(false);
                continue;
            };
            c.changed = differs(change);
            let known = self.touched().get(&c.path).cloned();
            let touched = match known {
                Some(touched) => touched,
                None if i < FACT_DIFFS && !cancel.load(Ordering::Relaxed) => {
                    let Some(touched) = self.diff_touched(repo, change) else {
                        continue;
                    };
                    let touched = Arc::new(touched);
                    self.touched().insert(c.path.clone(), Arc::clone(&touched));
                    touched
                }
                None => continue,
            };
            c.in_hunk = Some(touched.hits(c.start, c.end));
        }
    }

    fn touched(&self) -> MutexGuard<'_, FxHashMap<BString, Arc<Touched>>> {
        self.touched.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The lines `change`'s diff touches on this side; `None` when it
    /// can't be diffed.
    fn diff_touched(&self, repo: &Repo, change: &FileChange) -> Option<Touched> {
        let diff = repo.file_diff(change.clone(), &self.options).ok()?;
        let mut touched = Touched::default();
        for line in diff.hunks.iter().flat_map(|h| &h.lines) {
            match (line.kind, self.side) {
                (LineKind::Added, view::Side::New) => touched.lines.push(line.new),
                (LineKind::Removed, view::Side::Old) => touched.lines.push(line.old),
                (LineKind::Removed, view::Side::New) => touched.between.push(line.new),
                (LineKind::Added, view::Side::Old) => touched.between.push(line.old),
                (LineKind::Context, _) => {}
            }
        }
        for v in [&mut touched.lines, &mut touched.between] {
            v.sort_unstable();
            v.dedup();
        }
        Some(touched)
    }

    /// The files close to `origin`, worked out once per path.
    fn related(
        &self,
        repo: &Repo,
        origin: &Origin,
        cancel: &AtomicBool,
    ) -> Result<Arc<Related>, Error> {
        if let Some(known) = self.state().related.get(&origin.path) {
            return Ok(Arc::clone(known));
        }
        let mut related = Related::default();
        if let Some(l) = self.find(&origin.path) {
            related.files.push((l, 0));
            let text = self.text(repo, l).unwrap_or_default();
            related.words = words(&text);
            match origin.language.name() {
                "c" | "cpp" => self.included(repo, l, &mut related, cancel)?,
                "rust" => {
                    for l in self.used(origin.path.as_ref(), &text) {
                        if !related.files.iter().any(|f| f.0 == l) {
                            related.files.push((l, 1));
                        }
                    }
                }
                language => {
                    for l in self.imported(repo, l, language, &text) {
                        related.files.push((l, 1));
                    }
                }
            }
            self.follow_reexports(repo, origin.language.name(), &mut related);
        }
        let related = Arc::new(related);
        (self.state().related).insert(origin.path.clone(), Arc::clone(&related));
        Ok(related)
    }

    fn text(&self, repo: &Repo, l: u32) -> Option<Vec<u8>> {
        let listed = &self.listed[l as usize];
        repo.load(listed.path.as_ref(), &listed.side).ok()
    }

    /// The files `from` includes, directly or through each other, nearest
    /// first, up to [`MAX_INCLUDED`], each `#include` found where a
    /// compiler of `from` looks first (see [`Navigator::resolve_include`]).
    fn included(
        &self,
        repo: &Repo,
        from: u32,
        related: &mut Related,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        let arch = preferred_arch(self.listed[from as usize].path.as_ref());
        let arch = [b"arch/", arch, b"/include/"].concat();
        let arch = self.arches.iter().find(|a| a.as_slice() == arch.as_slice());
        let mut seen: FxHashSet<u32> = std::iter::once(from).collect();
        let mut next = 0;
        let mut queue = vec![(from, 0u8)];
        while let Some(&(l, depth)) = queue.get(next) {
            next += 1;
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let Some(text) = self.text(repo, l) else {
                continue;
            };
            let path = self.listed[l as usize].path.clone();
            for name in includes(&text) {
                let Some(found) = self.resolve_include(path.as_ref(), name, arch) else {
                    continue;
                };
                if seen.len() > MAX_INCLUDED {
                    return Ok(());
                }
                if seen.insert(found) {
                    let depth = depth.saturating_add(1);
                    related.files.push((found, depth));
                    queue.push((found, depth));
                }
            }
        }
        Ok(())
    }

    /// The file an `#include` of `name` in `from` finds first: next to
    /// `from`, then in the `include/` of each folder above it, nearest
    /// first (so `tools/` finds `tools/include/`), then in `arch`'s, then
    /// in the root's `include/`, then from the root.
    fn resolve_include(&self, from: &BStr, name: &[u8], arch: Option<&BString>) -> Option<u32> {
        let dir = folder(from);
        let mut bases: Vec<BString> = vec![dir.to_owned()];
        let mut up = dir;
        while !up.is_empty() {
            bases.push([up.as_bytes(), b"include/"].concat().into());
            up = folder(up[..up.len() - 1].as_bstr());
        }
        bases.extend(arch.cloned());
        bases.extend([BString::from("include/"), BString::default()]);
        bases
            .iter()
            .find_map(|base| self.find(&joined(base.as_ref(), name)?))
    }

    /// The crate folder of `path`: the deepest one holding a `Cargo.toml`
    /// above it.
    fn crate_of(&self, path: &BStr) -> Option<&BStr> {
        (self.crates.iter())
            .filter(|c| path.starts_with(c))
            .max_by_key(|c| c.len())
            .map(|c| c.as_bstr())
    }

    /// The files the `use` declarations in `text`, a Rust file at `from`,
    /// name: within its crate (`crate::`, `self::`, `super::`), or another
    /// crate of the repository by its folder's name.
    fn used(&self, from: &BStr, text: &[u8]) -> Vec<u32> {
        let Some(krate) = self.crate_of(from) else {
            return Vec::new();
        };
        let src: BString = [krate.as_bytes(), b"src/"].concat().into();
        let own = module_folder(from);
        let mut out = Vec::new();
        for path in uses(text) {
            let Some((first, rest)) = path.split_first() else {
                continue;
            };
            let parent = |dir: &BString| -> BString {
                folder(dir.strip_suffix(b"/").unwrap_or(dir).as_bstr()).to_owned()
            };
            let (mut dir, src) = match first.as_slice() {
                b"crate" => (src.clone(), src.clone()),
                b"self" => (own.clone(), src.clone()),
                b"super" => (parent(&own), src.clone()),
                name => {
                    let Some(other) = self.crates.iter().find(|c| {
                        let base = c.strip_suffix(b"/").unwrap_or(c);
                        let base = base.rsplit(|&b| b == b'/').next().unwrap_or(base);
                        !base.is_empty() && base.replace(b"-", b"_") == name
                    }) else {
                        continue;
                    };
                    let src: BString = [other.as_bytes(), b"src/"].concat().into();
                    (src.clone(), src)
                }
            };
            let mut rest = rest;
            while first.as_slice() == b"super" && rest.first().is_some_and(|s| s == "super") {
                dir = parent(&dir);
                rest = &rest[1..];
            }
            let mut deepest = self.module_file(dir.as_ref(), src.as_ref());
            for segment in rest {
                let file = [dir.as_bytes(), segment, b".rs"].concat();
                let module = [dir.as_bytes(), segment, b"/mod.rs"].concat();
                let Some(l) = self.find(&file).or_else(|| self.find(&module)) else {
                    break;
                };
                deepest = Some(l);
                dir = [dir.as_bytes(), segment, b"/"].concat().into();
            }
            if let Some(l) = deepest
                && !out.contains(&l)
            {
                out.push(l);
            }
        }
        out
    }

    /// The file of the Rust module whose children are in folder `dir`, in a
    /// crate whose sources are in `src`.
    fn module_file(&self, dir: &BStr, src: &BStr) -> Option<u32> {
        if dir == src {
            return (self.find(&[dir.as_bytes(), b"lib.rs"].concat()))
                .or_else(|| self.find(&[dir.as_bytes(), b"main.rs"].concat()));
        }
        let named = [dir.strip_suffix(b"/").unwrap_or(dir), b".rs"].concat();
        (self.find(&[dir.as_bytes(), b"mod.rs"].concat())).or_else(|| self.find(&named))
    }
}

/// A chunk of the index to build: the files of one of the symbol cache's
/// packs, or a run of working-tree files.
#[derive(Debug)]
pub struct Chunk {
    pack: Option<usize>,
    work: Vec<Work>,
    shared: Shared,
}

#[derive(Debug)]
struct Work {
    file: u32,
    path: BString,
    side: Side,
    key: Option<ObjectId>,
    language: Language,
}

/// A chunk's result, for [`Navigator::add`].
#[derive(Debug)]
pub struct Done {
    pack: Option<usize>,
    found: Vec<(u32, Option<Arc<Entry>>)>,
    rest: Vec<u32>,
    stats: BuildStats,
    error: Option<TagsError>,
}

impl Chunk {
    /// How many files it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.work.len()
    }

    /// Whether it holds none; a chunk from [`Navigator::take_chunk`] never
    /// does.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.work.is_empty()
    }

    /// Reads its files' definitions from the shared store or their pack,
    /// or extracts and stores those it lacks, stopping between files once
    /// `cancel` is set; the pack isn't read until a file needs it. `repo`
    /// should be a handle without an object cache
    /// ([`Repo::without_object_cache`]): each blob is read once.
    #[must_use]
    pub fn run(self, repo: &Repo, cache: &SymbolCache, cancel: &AtomicBool) -> Done {
        self.run_until(repo, cache, || cancel.load(Ordering::Relaxed))
    }

    /// [`Chunk::run`], stopping between files once `stop` says so: for a
    /// host that yields the build to other work as well as cancelling it.
    #[must_use]
    pub fn run_until(
        self,
        repo: &Repo,
        cache: &SymbolCache,
        mut stop: impl FnMut() -> bool,
    ) -> Done {
        let mut pack = self.pack.map(|p| LazyPack::new(cache, p));
        let mut done = Done {
            pack: self.pack,
            found: Vec::with_capacity(self.work.len()),
            rest: Vec::new(),
            stats: BuildStats::default(),
            error: None,
        };
        let mut work = self.work.into_iter();
        for w in work.by_ref() {
            if stop() {
                done.rest.push(w.file);
                break;
            }
            let budget = highlight::TAGS_PARSE_BUDGET;
            match w.entry(repo, pack.as_mut(), &self.shared, budget, &mut done.stats) {
                Ok(Got::Entry(entry)) => done.found.push((w.file, entry)),
                Ok(Got::TimedOut) => {
                    done.stats.timed_out += 1;
                    done.found.push((w.file, None));
                }
                Err(e) => {
                    done.error = done.error.or(Some(e));
                    done.found.push((w.file, None));
                }
            }
        }
        done.rest.extend(work.map(|w| w.file));
        if let Some(pack) = &mut pack {
            pack.write();
        }
        done
    }
}

/// A pack read when a file first needs it.
struct LazyPack<'c> {
    cache: &'c SymbolCache,
    index: usize,
    pack: Option<Pack>,
}

impl<'c> LazyPack<'c> {
    fn new(cache: &'c SymbolCache, index: usize) -> Self {
        Self {
            cache,
            index,
            pack: None,
        }
    }

    fn get(&mut self) -> &mut Pack {
        let (cache, index) = (self.cache, self.index);
        self.pack
            .get_or_insert_with(|| Pack::read_index(cache, index))
    }

    fn write(&mut self) {
        if let Some(pack) = &mut self.pack {
            pack.write(self.cache);
        }
    }
}

/// What reading a file's definitions came to.
enum Got {
    /// `None` for a file with none to give.
    Entry(Option<Arc<Entry>>),
    /// Its parse took longer than it was given.
    TimedOut,
}

impl Work {
    /// The file's definitions: from `shared` or `pack` when they have them,
    /// else extracted with a parse given `budget`, and added to both.
    fn entry(
        &self,
        repo: &Repo,
        pack: Option<&mut LazyPack>,
        shared: &Shared,
        budget: Duration,
        stats: &mut BuildStats,
    ) -> Result<Got, TagsError> {
        if let Some(entry) = self.key.and_then(|key| shared.get(&key)) {
            stats.cached += 1;
            return Ok(Got::Entry(Some(entry)));
        }
        let mut stored = self.key.zip(pack);
        if let Some((key, pack)) = &mut stored
            && let Some(raw) = pack.get().get(key)
            && let Some(found) = highlight::cached_definitions(raw)
        {
            stats.cached += 1;
            let Ok(defs) = found else {
                stats.too_large += 1;
                return Ok(Got::Entry(None));
            };
            let entry = Arc::new(Entry::new(&defs, raw.into()));
            shared.insert(*key, &entry);
            return Ok(Got::Entry(Some(entry)));
        }
        let Ok(size) = repo.size(self.path.as_ref(), &self.side) else {
            stats.unreadable += 1;
            return Ok(Got::Entry(None));
        };
        let extracted = highlight::extract_blob(&self.language, size, budget, || {
            repo.load(self.path.as_ref(), &self.side).ok()
        })?;
        let Some((entry, found)) = extracted else {
            stats.unreadable += 1;
            return Ok(Got::Entry(None));
        };
        if let (Some((key, pack)), Some(entry)) = (&mut stored, &entry) {
            pack.get().insert(*key, entry.clone());
        }
        Ok(match found {
            Ok(defs) => {
                stats.extracted += 1;
                let encoded = entry.unwrap_or_else(|| defs.encode());
                let entry = Arc::new(Entry::new(&defs, encoded.into()));
                if let Some(key) = self.key {
                    shared.insert(key, &entry);
                }
                Got::Entry(Some(entry))
            }
            Err(Skipped::TooLarge) => {
                stats.too_large += 1;
                Got::Entry(None)
            }
            Err(Skipped::TimedOut) => Got::TimedOut,
        })
    }
}

/// The architecture whose headers `path` includes: its own under `arch/`,
/// else [`DEFAULT_ARCH`].
fn preferred_arch(path: &BStr) -> &[u8] {
    path.strip_prefix(b"arch/")
        .and_then(|rest| rest.split_once_str("/"))
        .map_or(DEFAULT_ARCH, |(arch, _)| arch)
}

/// Whether `path` is another architecture's than `arch`.
fn foreign_arch(path: &BStr, arch: &[u8]) -> bool {
    path.strip_prefix(b"arch/")
        .and_then(|rest| rest.split_once_str("/"))
        .is_some_and(|(other, _)| other != arch)
}

/// The indexed files of `listed` (sorted by path) among `changes`.
fn diffed(listed: &[Listed], changes: &FxHashMap<BString, FileChange>) -> Vec<u32> {
    let mut files: Vec<u32> = (changes.keys())
        .filter_map(|path| {
            let l = listed
                .binary_search_by(|l| l.path.as_slice().cmp(path))
                .ok()?;
            listed[l].indexed
        })
        .collect();
    files.sort_unstable();
    files
}

/// `changes` by their path on `side`.
fn by_path(changes: &[FileChange], side: view::Side) -> FxHashMap<BString, FileChange> {
    (changes.iter())
        .map(|c| (on_side(c, side).0, c.clone()))
        .collect()
}

/// Where `change` is on `side`: its path there and that side.
fn on_side(change: &FileChange, side: view::Side) -> (BString, Option<Side>) {
    match side {
        view::Side::Old => (change.old_path().to_owned(), change.old),
        view::Side::New => (change.path.clone(), change.new),
    }
}

/// The files at `side` of a diff with `snapshots` and `changes`, by path:
/// for the working tree, the other snapshot's with the changes laid over.
fn side_files(
    repo: &Repo,
    snapshots: [Snapshot; 2],
    side: view::Side,
    changes: &[FileChange],
) -> Result<Vec<(BString, Side)>, git::Error> {
    let at = snapshots[side.index()];
    let base = match at {
        Snapshot::Worktree => snapshots[side.other().index()],
        at => at,
    };
    let mut files: Vec<(BString, Side)> = Vec::new();
    repo.list_sides(base, &[], |path, side| {
        files.push((path.to_owned(), side));
        std::ops::ControlFlow::Continue(())
    })?;
    if at == Snapshot::Worktree && base != Snapshot::Worktree {
        let gone: FxHashSet<BString> = (changes.iter())
            .filter_map(|c| {
                let (path, other) = on_side(c, side.other());
                other.map(|_| path)
            })
            .chain(changes.iter().map(|c| on_side(c, side).0))
            .collect();
        files.retain(|(path, _)| !gone.contains(path));
        files.extend(changes.iter().filter_map(|c| {
            let (path, here) = on_side(c, side);
            Some((path, here?))
        }));
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files.dedup_by(|b, a| a.0 == b.0);
    }
    Ok(files)
}

/// The index build's chunks: a symbol cache pack's files each, and the
/// working tree's changed files, which run first.
fn queue(indexed: &[Indexed]) -> Vec<Queued> {
    let mut packs: Vec<Vec<u32>> = vec![Vec::new(); highlight::PACKS];
    let mut loose = Vec::new();
    for (i, file) in indexed.iter().enumerate() {
        match &file.key {
            Some(key) => packs[highlight::pack_of(key)].push(to_u32(i)),
            None => loose.push(to_u32(i)),
        }
    }
    let mut queue: Vec<Queued> = (packs.into_iter().enumerate().rev())
        .filter(|(_, files)| !files.is_empty())
        .map(|(pack, files)| Queued {
            pack: Some(pack),
            files,
        })
        .collect();
    queue.extend(loose.chunks(OVERLAY_CHUNK).rev().map(|files| Queued {
        pack: None,
        files: files.to_vec(),
    }));
    queue
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The folder of `path` with its trailing `/`; empty at the root.
fn folder(path: &BStr) -> &BStr {
    path.rfind_byte(b'/')
        .map_or_else(|| b"".as_bstr(), |i| path[..=i].as_bstr())
}

/// How many leading folders two paths share.
fn shared_folders(a: &[u8], b: &[u8]) -> usize {
    let (a, b) = (folder(a.as_bstr()), folder(b.as_bstr()));
    (a.split(|&c| c == b'/').zip(b.split(|&c| c == b'/')))
        .take_while(|(x, y)| x == y && !x.is_empty())
        .count()
}

/// `name` under folder `base`, with `.` and `..` worked out; `None` when it
/// leaves the root.
fn joined(base: &BStr, name: &[u8]) -> Option<BString> {
    let mut parts: Vec<&[u8]> = base
        .split(|&b| b == b'/')
        .filter(|p| !p.is_empty())
        .collect();
    for part in name.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join(&b'/').into())
}

/// The names of the `#include "…"` and `#include <…>` lines of `text`.
fn includes(text: &[u8]) -> impl Iterator<Item = &[u8]> {
    text.lines().filter_map(|line| {
        let rest = line.trim_start().strip_prefix(b"#")?.trim_start();
        let rest = rest.strip_prefix(b"include")?.trim_start();
        let close = match rest.first()? {
            b'"' => b'"',
            b'<' => b'>',
            _ => return None,
        };
        let name = &rest[1..];
        Some(&name[..name.find_byte(close)?])
    })
}

/// The folder a Rust file's child modules are in: its own for `lib.rs`,
/// `main.rs` and `mod.rs`, else one named after it.
fn module_folder(path: &BStr) -> BString {
    let dir = folder(path);
    let name = &path[dir.len()..];
    match name.strip_suffix(b".rs") {
        Some(b"lib" | b"main" | b"mod") | None => dir.to_owned(),
        Some(stem) => [dir.as_bytes(), stem, b"/"].concat().into(),
    }
}

/// The paths the `use` declarations of `text` name, each as its segments:
/// `use a::{b, c::d as e};` is `a::b` and `a::c::d`; a glob or `self` names
/// the module it's in.
fn uses(text: &[u8]) -> Vec<Vec<BString>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = memchr::memmem::find(rest, b"use ") {
        let starts = at == 0 || matches!(rest[at - 1], b'\n' | b' ' | b'\t' | b')' | b';' | b'{');
        rest = &rest[at + 4..];
        if !starts {
            continue;
        }
        let Some(end) = rest.find_byte(b';') else {
            break;
        };
        expand(&rest[..end], &mut Vec::new(), &mut out);
        rest = &rest[end..];
    }
    out
}

/// Expands a use tree under `prefix`.
fn expand(tree: &[u8], prefix: &mut Vec<BString>, out: &mut Vec<Vec<BString>>) {
    let tree = tree.trim();
    let len = prefix.len();
    let (path, group) = match tree.find_byte(b'{') {
        Some(open) if tree.ends_with(b"}") => {
            (&tree[..open], Some(&tree[open + 1..tree.len() - 1]))
        }
        _ => (tree, None),
    };
    for segment in path.split_str("::") {
        // `d as e` is `d`.
        let Some(segment) = segment.fields().next() else {
            continue;
        };
        if !matches!(segment, b"*" | b"self") {
            prefix.push(segment.into());
        }
    }
    match group {
        Some(group) => {
            let mut depth = 0;
            let mut start = 0;
            for (i, &b) in group.iter().enumerate() {
                match b {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    b',' if depth == 0 => {
                        expand(&group[start..i], prefix, out);
                        start = i + 1;
                    }
                    _ => {}
                }
            }
            expand(&group[start..], prefix, out);
        }
        None => out.push(prefix.clone()),
    }
    prefix.truncate(len);
}

#[cfg(test)]
mod tests;

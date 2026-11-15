//! Rename detection as a pass over an enumerated change list, following
//! git's diffcore-rename (v2.43) so the pairs and scores match `git diff -M`:
//!
//! 1. Exact renames: a deletion and an addition with the same blob id,
//!    preferring an unused source with the same file name.
//! 2. File-name renames: when a file name is unique among the remaining
//!    deletions and among the remaining additions, the two pair if they
//!    score at least 75%.
//! 3. The rest, if there are at most [`RENAME_LIMIT`]² source and
//!    destination combinations: every pair scores, each destination keeps
//!    its best four, and the best-scoring pairs (same file name first among
//!    equals) are taken greedily down to 50%.
//!
//! A score is the bytes of the destination that also occur in the source,
//! counted over git's line-or-64-byte chunk hashes, over the larger size.
//! Pairs whose sizes differ too much to reach the threshold are never
//! loaded. Loading and hashing blobs, and the score matrix, run on all
//! cores.

use std::cmp::Ordering;
use std::sync::atomic::{self, AtomicUsize};

use gix::bstr::{BStr, ByteSlice};
use rustc_hash::FxHashMap;

use super::{FileChange, Mode, ObjectId, Repo, Result, Side, Staging, Status, is_binary};
use crate::cache::{self, Cache};

/// git's `diff.renameLimit` default: inexact detection is skipped when
/// sources × destinations exceeds its square.
pub(super) const RENAME_LIMIT: usize = 1000;

const MAX_SCORE: u64 = 60_000;
/// git's default `-M50%`.
const MIN_SCORE: u64 = MAX_SCORE / 2;
const BASENAME_SCORE: u64 = MIN_SCORE + (MAX_SCORE - MIN_SCORE) / 2;
const CANDIDATES_PER_DST: usize = 4;
/// Identical sources examined per destination before taking the best so far.
const MAX_IDENTICAL: usize = 100;
/// git's chunk hash modulus, a prime between 2^16 and 2^17.
const HASHBASE: u32 = 107_927;

/// A deletion or addition taking part in the pass.
#[derive(Clone, Copy)]
struct File<'a> {
    /// Index into the change list.
    index: usize,
    path: &'a BStr,
    side: Side,
    id: Option<ObjectId>,
    size: u64,
}

impl File<'_> {
    fn regular(&self) -> bool {
        matches!(self.side.mode(), Mode::File | Mode::Executable)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pair {
    src: usize,
    dst: usize,
    score: u64,
}

impl Repo {
    /// Pairs deletions with additions in `changes` (in enumeration order,
    /// as from [`Repo::for_each_change`], already reversed for `-R`) and
    /// returns the list with each pair merged into one
    /// [`Status::Renamed`] entry where the addition was. Untracked files
    /// take no part. `cache` keeps whole results by their inputs.
    ///
    /// # Errors
    ///
    /// Object database and working-tree read failures.
    pub fn detect_renames(
        &self,
        changes: Vec<FileChange>,
        cache: Option<&Cache>,
    ) -> Result<Vec<FileChange>> {
        let pairs = self.rename_pairs(&changes, cache)?;
        if pairs.is_empty() {
            return Ok(changes);
        }
        let mut src_of = vec![None; changes.len()];
        let mut used = vec![false; changes.len()];
        for p in &pairs {
            src_of[p.dst] = Some(p);
            used[p.src] = true;
        }
        let mut sources: Vec<Option<FileChange>> = Vec::with_capacity(changes.len());
        let mut kept = Vec::with_capacity(changes.len());
        for (i, change) in changes.into_iter().enumerate() {
            if used[i] {
                sources.push(Some(change));
            } else {
                sources.push(None);
                kept.push((i, change));
            }
        }
        Ok(kept
            .into_iter()
            .map(|(i, change)| {
                let Some((p, src)) = src_of[i].and_then(|p| Some((p, sources[p.src].take()?)))
                else {
                    return change;
                };
                FileChange {
                    path: change.path,
                    status: Status::Renamed {
                        from: src.path,
                        similarity: u8::try_from(p.score * 100 / MAX_SCORE).unwrap_or(100),
                    },
                    old: src.old,
                    new: change.new,
                    staging: match (src.staging, change.staging) {
                        (Some(a), Some(b)) if a != b => Some(Staging::Partly),
                        (a, b) => b.or(a),
                    },
                }
            })
            .collect())
    }

    fn rename_pairs(&self, changes: &[FileChange], cache: Option<&Cache>) -> Result<Vec<Pair>> {
        let mut srcs = Vec::new();
        let mut dsts = Vec::new();
        for (index, change) in changes.iter().enumerate() {
            let (list, side) = match (&change.status, change.old, change.new) {
                (Status::Deleted, Some(side), _) => (&mut srcs, side),
                (Status::Added, _, Some(side)) => (&mut dsts, side),
                _ => continue,
            };
            list.push(File {
                index,
                path: change.path.as_bstr(),
                side,
                id: None,
                size: 0,
            });
        }
        if srcs.is_empty() || dsts.is_empty() {
            return Ok(Vec::new());
        }
        for file in srcs.iter_mut().chain(dsts.iter_mut()) {
            file.id = self.blob_id(file.path, &file.side)?;
        }
        let key = cache.map(|_| cache_key(&srcs, &dsts));
        if let (Some(cache), Some(key)) = (cache, &key)
            && let Some(pairs) = cache
                .get(cache::Kind::Renames, key)
                .and_then(|bytes| decode_pairs(&bytes, changes.len()))
        {
            return Ok(pairs);
        }
        let pairs = self.find_pairs(&srcs, &dsts)?;
        if let (Some(cache), Some(key)) = (cache, &key) {
            cache.put(cache::Kind::Renames, key, &encode_pairs(&pairs));
        }
        Ok(pairs)
    }

    /// The blob id of a side, hashing working-tree content when the scan
    /// didn't. `None` for a working-tree submodule, which has no blob.
    fn blob_id(&self, path: &BStr, side: &Side) -> Result<Option<ObjectId>> {
        Ok(match *side {
            Side::Object { id, .. } | Side::Worktree { id: Some(id), .. } => Some(id),
            Side::Worktree {
                mode: Mode::Gitlink,
                ..
            } => None,
            Side::Worktree { .. } | Side::Path { .. } => {
                let data = self.load(path, side)?;
                gix::objs::compute_hash(self.object_hash(), gix::objs::Kind::Blob, &data).ok()
            }
        })
    }

    fn find_pairs<'a>(&self, srcs: &[File<'a>], dsts: &[File<'a>]) -> Result<Vec<Pair>> {
        let mut pairs = Vec::new();
        let (srcs_left, dsts_left) = exact_pairs(srcs, dsts, &mut pairs);
        if srcs_left.is_empty() || dsts_left.is_empty() {
            return Ok(pairs);
        }
        let sized = |file: &File<'a>| -> Result<File<'a>> {
            Ok(File {
                size: self.size(file.path, &file.side)?,
                ..*file
            })
        };
        let srcs = srcs_left
            .iter()
            .map(|&s| sized(&srcs[s]))
            .collect::<Result<Vec<_>>>()?;
        let dsts = dsts_left
            .iter()
            .map(|&d| sized(&dsts[d]))
            .collect::<Result<Vec<_>>>()?;
        let mut inexact = Inexact {
            src_used: vec![false; srcs.len()],
            dst_done: vec![false; dsts.len()],
            src_prints: vec![None; srcs.len()],
            dst_prints: vec![None; dsts.len()],
            srcs,
            dsts,
            pairs,
        };
        inexact.by_name(self)?;
        inexact.matrix(self)?;
        Ok(inexact.pairs)
    }
}

/// git's exact pass: each destination takes the first unused source with
/// the same blob (and, unless both are regular files, the same mode),
/// preferring one with the same file name. Returns the sources and
/// destinations left over.
fn exact_pairs(
    srcs: &[File<'_>],
    dsts: &[File<'_>],
    pairs: &mut Vec<Pair>,
) -> (Vec<usize>, Vec<usize>) {
    let mut src_used = vec![false; srcs.len()];
    let mut dst_done = vec![false; dsts.len()];
    let mut by_id: FxHashMap<ObjectId, Vec<usize>> = FxHashMap::default();
    for (s, file) in srcs.iter().enumerate() {
        if let Some(id) = file.id {
            by_id.entry(id).or_default().push(s);
        }
    }
    for (d, dst) in dsts.iter().enumerate() {
        let Some(candidates) = dst.id.and_then(|id| by_id.get(&id)) else {
            continue;
        };
        let mut best: Option<(usize, u8)> = None;
        let mut examined = 0;
        for &s in candidates {
            let src = &srcs[s];
            let same_kind = (src.regular() && dst.regular()) || src.side.mode() == dst.side.mode();
            if src_used[s] || !same_kind {
                continue;
            }
            let score = 1 + u8::from(basename(src.path) == basename(dst.path));
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((s, score));
                if score == 2 {
                    break;
                }
            }
            examined += 1;
            if examined == MAX_IDENTICAL {
                break;
            }
        }
        if let Some((s, _)) = best {
            src_used[s] = true;
            dst_done[d] = true;
            pairs.push(Pair {
                src: srcs[s].index,
                dst: dst.index,
                score: MAX_SCORE,
            });
        }
    }
    (
        (0..srcs.len()).filter(|&s| !src_used[s]).collect(),
        (0..dsts.len()).filter(|&d| !dst_done[d]).collect(),
    )
}

/// The inexact passes over what the exact pass left, with sizes known.
/// Files are fingerprinted only once a pass needs them, so blobs are loaded
/// only for pairs whose sizes could reach a pass's threshold, and not at all
/// when the matrix is over [`RENAME_LIMIT`].
struct Inexact<'a> {
    srcs: Vec<File<'a>>,
    dsts: Vec<File<'a>>,
    src_prints: Vec<Option<Fingerprint>>,
    dst_prints: Vec<Option<Fingerprint>>,
    src_used: Vec<bool>,
    dst_done: Vec<bool>,
    pairs: Vec<Pair>,
}

impl Inexact<'_> {
    /// git's `estimate_similarity`: 0 unless both are regular files of
    /// sizes close enough to reach `minimum`. Both must be fingerprinted
    /// when they are.
    fn score(&self, s: usize, d: usize, minimum: u64) -> u64 {
        let (src, dst) = (&self.srcs[s], &self.dsts[d]);
        if !may_reach(src, dst, minimum) || dst.size == 0 {
            return 0;
        }
        let empty = Fingerprint::default();
        let sp = self.src_prints[s].as_ref().unwrap_or(&empty);
        let dp = self.dst_prints[d].as_ref().unwrap_or(&empty);
        sp.shared(dp) * MAX_SCORE / src.size.max(dst.size)
    }

    /// Fingerprints the sources `srcs` and destinations `dsts` that aren't
    /// yet, on all cores.
    fn load(&mut self, repo: &Repo, srcs: &[usize], dsts: &[usize]) -> Result<()> {
        let wanted: Vec<(bool, usize)> = srcs
            .iter()
            .filter(|&&s| self.src_prints[s].is_none())
            .map(|&s| (true, s))
            .chain(
                dsts.iter()
                    .filter(|&&d| self.dst_prints[d].is_none())
                    .map(|&d| (false, d)),
            )
            .collect();
        let (srcs_ref, dsts_ref) = (&self.srcs, &self.dsts);
        let prints = parallel_map(
            &wanted,
            || repo.clone(),
            |repo, &(is_src, i)| -> Result<Fingerprint> {
                let file = if is_src { &srcs_ref[i] } else { &dsts_ref[i] };
                Ok(Fingerprint::of(&repo.load(file.path, &file.side)?))
            },
        );
        for (&(is_src, i), print) in wanted.iter().zip(prints) {
            let slot = if is_src {
                &mut self.src_prints[i]
            } else {
                &mut self.dst_prints[i]
            };
            *slot = Some(print?);
        }
        Ok(())
    }

    fn pair(&mut self, s: usize, d: usize, score: u64) {
        self.src_used[s] = true;
        self.dst_done[d] = true;
        self.pairs.push(Pair {
            src: self.srcs[s].index,
            dst: self.dsts[d].index,
            score,
        });
    }

    /// Pairs whose file name is unique among the sources and among the
    /// destinations, if they score at least [`BASENAME_SCORE`].
    fn by_name(&mut self, repo: &Repo) -> Result<()> {
        let (src_names, dst_names) = (unique_names(&self.srcs), unique_names(&self.dsts));
        let candidates: Vec<(usize, usize)> = (0..self.srcs.len())
            .filter_map(|s| {
                let name = basename(self.srcs[s].path);
                match (src_names.get(name), dst_names.get(name)) {
                    (Some(Some(_)), Some(&Some(d))) => Some((s, d)),
                    _ => None,
                }
            })
            .filter(|&(s, d)| may_reach(&self.srcs[s], &self.dsts[d], BASENAME_SCORE))
            .collect();
        let (srcs, dsts): (Vec<usize>, Vec<usize>) = candidates.iter().copied().unzip();
        self.load(repo, &srcs, &dsts)?;
        for (s, d) in candidates {
            if self.dst_done[d] {
                continue;
            }
            let score = self.score(s, d, BASENAME_SCORE);
            if score >= BASENAME_SCORE {
                self.pair(s, d, score);
            }
        }
        Ok(())
    }

    /// Everything left against everything left, within [`RENAME_LIMIT`].
    fn matrix(&mut self, repo: &Repo) -> Result<()> {
        let srcs_left: Vec<usize> = (0..self.srcs.len())
            .filter(|&s| !self.src_used[s])
            .collect();
        let dsts_left: Vec<usize> = (0..self.dsts.len())
            .filter(|&d| !self.dst_done[d])
            .collect();
        if srcs_left.is_empty()
            || dsts_left.is_empty()
            || srcs_left.len() * dsts_left.len() > RENAME_LIMIT * RENAME_LIMIT
        {
            return Ok(());
        }
        let sizes = |files: &[File<'_>], left: &[usize]| {
            let mut sizes: Vec<u64> = left
                .iter()
                .map(|&i| &files[i])
                .filter(|f| f.regular())
                .map(|f| f.size)
                .collect();
            sizes.sort_unstable();
            sizes
        };
        let (src_sizes, dst_sizes) = (sizes(&self.srcs, &srcs_left), sizes(&self.dsts, &dsts_left));
        // At 50% a pair can score only if neither size is more than twice
        // the other.
        let has_partner = |file: &File<'_>, others: &[u64]| {
            let first = others.partition_point(|&t| t < file.size.div_ceil(2));
            file.regular()
                && others
                    .get(first)
                    .is_some_and(|&t| t <= file.size.saturating_mul(2))
        };
        let load_srcs: Vec<usize> = srcs_left
            .iter()
            .copied()
            .filter(|&s| has_partner(&self.srcs[s], &dst_sizes))
            .collect();
        let load_dsts: Vec<usize> = dsts_left
            .iter()
            .copied()
            .filter(|&d| has_partner(&self.dsts[d], &src_sizes))
            .collect();
        self.load(repo, &load_srcs, &load_dsts)?;
        let this = &*self;
        let mut matrix = parallel_map(
            &dsts_left,
            || (),
            |(), &d| {
                let mut best: [Option<Candidate>; CANDIDATES_PER_DST] = [None; CANDIDATES_PER_DST];
                for &s in &srcs_left {
                    let candidate = Candidate {
                        src: s,
                        dst: d,
                        score: this.score(s, d, MIN_SCORE),
                        same_name: basename(this.srcs[s].path) == basename(this.dsts[d].path),
                    };
                    record_if_better(&mut best, candidate);
                }
                best
            },
        )
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        // Stable, as git's: equal candidates keep destination order.
        matrix.sort_by(|a, b| compare(a.as_ref(), b.as_ref()));
        for c in matrix.into_iter().flatten() {
            if c.score < MIN_SCORE {
                break;
            }
            if !self.dst_done[c.dst] && !self.src_used[c.src] {
                self.pair(c.src, c.dst, c.score);
            }
        }
        Ok(())
    }
}

/// Each file name to its file's index, or `None` when several share it.
fn unique_names<'p>(files: &[File<'p>]) -> FxHashMap<&'p [u8], Option<usize>> {
    let mut map: FxHashMap<&[u8], Option<usize>> = FxHashMap::default();
    for (i, f) in files.iter().enumerate() {
        map.entry(basename(f.path))
            .and_modify(|e| *e = None)
            .or_insert(Some(i));
    }
    map
}

fn basename(path: &BStr) -> &[u8] {
    path.rsplit_str("/").next().unwrap_or(path)
}

/// git's chunk hashes of a file: each line, or each 64 bytes of a longer
/// one, hashes into one of [`HASHBASE`] buckets weighted by its length.
/// Sorted by hash, with equal hashes merged.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Fingerprint(Vec<(u32, u32)>);

impl Fingerprint {
    fn of(data: &[u8]) -> Self {
        let text = !is_binary(data);
        let mut chunks: Vec<(u32, u32)> = Vec::with_capacity(data.len() / 32 + 1);
        let (mut accum1, mut accum2, mut n) = (0u32, 0u32, 0u32);
        for (i, &c) in data.iter().enumerate() {
            // CR of a CRLF pair doesn't count, so line endings don't
            // affect the score.
            if text && c == b'\r' && data.get(i + 1) == Some(&b'\n') {
                continue;
            }
            let old = accum1;
            accum1 = (accum1 << 7) ^ (accum2 >> 25);
            accum2 = (accum2 << 7) ^ (old >> 25);
            accum1 = accum1.wrapping_add(u32::from(c));
            n += 1;
            if n < 64 && c != b'\n' {
                continue;
            }
            let hash = accum1.wrapping_add(accum2.wrapping_mul(0x61)) % HASHBASE;
            chunks.push((hash, n));
            (accum1, accum2, n) = (0, 0, 0);
        }
        // git v2.43 drops a trailing chunk without a newline, and so do we
        // to score the same; later git counts it, which moves scores of
        // files without a final newline by a point or so.
        chunks.sort_unstable_by_key(|c| c.0);
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(chunks.len());
        for (hash, n) in chunks {
            match merged.last_mut() {
                Some(last) if last.0 == hash => last.1 += n,
                _ => merged.push((hash, n)),
            }
        }
        Self(merged)
    }

    /// Bytes of `other` that also occur here, chunk for chunk.
    fn shared(&self, other: &Self) -> u64 {
        let (a, b) = (&self.0, &other.0);
        let (mut i, mut j, mut shared) = (0, 0, 0u64);
        while i < a.len() && j < b.len() {
            match a[i].0.cmp(&b[j].0) {
                Ordering::Less => i += 1,
                Ordering::Greater => j += 1,
                Ordering::Equal => {
                    shared += u64::from(a[i].1.min(b[j].1));
                    i += 1;
                    j += 1;
                }
            }
        }
        shared
    }
}

/// Whether `src` and `dst` are regular files whose sizes are close enough
/// for their score to reach `minimum` (git's size test, before any load).
fn may_reach(src: &File<'_>, dst: &File<'_>, minimum: u64) -> bool {
    let max = src.size.max(dst.size);
    let delta = max - src.size.min(dst.size);
    src.regular() && dst.regular() && max * (MAX_SCORE - minimum) >= delta * MAX_SCORE
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    src: usize,
    dst: usize,
    score: u64,
    same_name: bool,
}

/// git's `score_compare`: better first, empty slots last; a same-name pair
/// beats an equal score.
fn compare(a: Option<&Candidate>, b: Option<&Candidate>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => b.score.cmp(&a.score).then(b.same_name.cmp(&a.same_name)),
    }
}

/// git's `record_if_better`: replaces the worst of the kept candidates when
/// `candidate` sorts strictly before it.
fn record_if_better(best: &mut [Option<Candidate>; CANDIDATES_PER_DST], candidate: Candidate) {
    let mut worst = 0;
    for i in 1..CANDIDATES_PER_DST {
        if compare(best[i].as_ref(), best[worst].as_ref()) == Ordering::Greater {
            worst = i;
        }
    }
    if compare(best[worst].as_ref(), Some(&candidate)) == Ordering::Greater {
        best[worst] = Some(candidate);
    }
}

/// Maps `items` on all cores, each thread with its own state made by
/// `init` (on this thread, so the state needn't be shareable). Results come
/// back in order.
fn parallel_map<T: Sync, S: Send, R: Send>(
    items: &[T],
    mut init: impl FnMut() -> S,
    f: impl Fn(&mut S, &T) -> R + Sync,
) -> Vec<R> {
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(items.len().div_ceil(16))
        .max(1);
    if threads == 1 {
        let mut state = init();
        return items.iter().map(|item| f(&mut state, item)).collect();
    }
    let next = AtomicUsize::new(0);
    let (next, f) = (&next, &f);
    let mut results: Vec<(usize, R)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let mut state = init();
                scope.spawn(move || {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, atomic::Ordering::Relaxed);
                        let Some(item) = items.get(i) else { break };
                        out.push((i, f(&mut state, item)));
                    }
                    out
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    });
    results.sort_unstable_by_key(|r| r.0);
    results.into_iter().map(|r| r.1).collect()
}

/// Everything a rename result depends on: the paths, ids and modes of both
/// lists, in order.
fn cache_key(srcs: &[File<'_>], dsts: &[File<'_>]) -> Vec<u8> {
    let mut input = Vec::new();
    for (tag, files) in [(b'S', srcs), (b'D', dsts)] {
        for f in files {
            input.push(tag);
            cache::put_varint(&mut input, f.index as u64);
            input.extend_from_slice(f.path);
            input.push(0);
            input.push(f.side.mode() as u8);
            match f.id {
                Some(id) => input.extend_from_slice(id.as_bytes()),
                None => input.push(0),
            }
        }
    }
    cache::digest(&input).as_bytes().to_vec()
}

fn encode_pairs(pairs: &[Pair]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pairs.len() * 6);
    for p in pairs {
        cache::put_varint(&mut out, p.src as u64);
        cache::put_varint(&mut out, p.dst as u64);
        cache::put_varint(&mut out, p.score);
    }
    out
}

fn decode_pairs(mut bytes: &[u8], len: usize) -> Option<Vec<Pair>> {
    let mut pairs = Vec::new();
    while !bytes.is_empty() {
        let mut get = || usize::try_from(cache::get_varint(&mut bytes)?).ok();
        let (src, dst, score) = (get()?, get()?, get()? as u64);
        if src >= len || dst >= len || score > MAX_SCORE {
            return None;
        }
        pairs.push(Pair { src, dst, score });
    }
    Some(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_follow_git_chunking() {
        // Two lines and a trailing partial chunk, which git ignores.
        let a = Fingerprint::of(b"one\ntwo\ntail");
        assert_eq!(a.0.iter().map(|c| c.1).sum::<u32>(), 8);
        // CRLF scores like LF in text, not in binary.
        assert_eq!(
            Fingerprint::of(b"one\r\ntwo\r\n"),
            Fingerprint::of(b"one\ntwo\n")
        );
        assert_ne!(Fingerprint::of(b"\0ne\r\n"), Fingerprint::of(b"\0ne\n"));
        // A long line splits every 64 bytes.
        let long = Fingerprint::of(&[b'x'; 130]);
        assert_eq!(long.0, [(long.0[0].0, 128)]);
        assert_eq!(a.shared(&a), 8);
    }

    #[test]
    fn candidates_keep_the_best_four_preferring_same_names() {
        let c = |src, score, same_name| Candidate {
            src,
            dst: 0,
            score,
            same_name,
        };
        let mut best = [None; CANDIDATES_PER_DST];
        for (i, score) in [10, 50, 20, 40, 30, 50].into_iter().enumerate() {
            record_if_better(&mut best, c(i, score, i == 5));
        }
        let mut kept = best.to_vec();
        kept.sort_by(|a, b| compare(a.as_ref(), b.as_ref()));
        let order: Vec<usize> = kept.iter().flatten().map(|c| c.src).collect();
        assert_eq!(order, [5, 1, 3, 4]);
    }
}

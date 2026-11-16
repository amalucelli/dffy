//! Structural diffs in the disk cache. A large file's diff takes tens to
//! hundreds of milliseconds to compute and about a millisecond to read
//! back, so it's kept by content: both blob ids, the language's identity
//! (its grammar and query), and the line hunks it was laid over.

use crate::cache::{self, Cache, Kind, get_varint, put_varint};
use crate::diff::{Hunk, Line, LineKind, Unchanged};
use crate::git::ObjectId;
use crate::highlight::Language;

use super::{Regions, Structural};

/// Files with a side at least this large are cached; smaller ones compute
/// faster than a disk read.
pub const CACHE_MIN_BYTES: usize = 64 << 10;

/// Bump when the matching changes what it produces.
const VERSION: u8 = 4;

/// What a cached structural diff depends on.
pub(super) fn key(lang: &Language, ids: [ObjectId; 2], hunks: &[Hunk]) -> Vec<u8> {
    let mut key = vec![VERSION];
    key.extend_from_slice(lang.identity().as_bytes());
    for id in ids {
        key.extend_from_slice(id.as_bytes());
    }
    // The hunks, which context and whitespace options shape.
    let mut shape = Vec::new();
    for h in hunks {
        for n in [h.old_start, h.old_len, h.new_start, h.new_len] {
            put_varint(&mut shape, n as u64);
        }
        shape.extend(h.lines.iter().map(|l| l.kind as u8));
    }
    key.extend_from_slice(cache::digest(&shape).as_bytes());
    key
}

pub(super) fn get(cache: &Cache, key: &[u8]) -> Option<Structural> {
    decode(&cache.get(Kind::Structural, key)?)
}

pub(super) fn put(cache: &Cache, key: &[u8], s: &Structural) {
    cache.put(Kind::Structural, key, &encode(s));
}

fn encode(s: &Structural) -> Vec<u8> {
    let mut out = Vec::new();
    let mut put = |n: usize| put_varint(&mut out, n as u64);
    let r = s.regions;
    put(r.matched);
    put(r.one_sided);
    put(r.fell_back);
    put(s.hunks.len());
    for h in &s.hunks {
        for n in [
            h.old_start,
            h.old_len,
            h.new_start,
            h.new_len,
            h.lines.len(),
        ] {
            put(n);
        }
        // Line numbers follow from the kinds, in order on both sides.
        for line in &h.lines {
            let unchanged = match line.unchanged {
                Unchanged::No => 0,
                Unchanged::Layout => 1,
                Unchanged::Moved => 2,
            };
            put(line.kind as usize
                | unchanged << 2
                | usize::from(line.block_start) << 4
                | usize::from(line.paired) << 5);
            put(line.emphasis.len());
            let mut at = 0;
            for e in &line.emphasis {
                put(e.start - at);
                put(e.end - e.start);
                at = e.end;
            }
        }
    }
    out
}

fn decode(bytes: &[u8]) -> Option<Structural> {
    let mut input = bytes;
    let mut get = || -> Option<usize> { usize::try_from(get_varint(&mut input)?).ok() };
    let regions = Regions {
        matched: get()?,
        one_sided: get()?,
        fell_back: get()?,
    };
    let count = get()?;
    let mut hunks = Vec::with_capacity(count.min(bytes.len()));
    for _ in 0..count {
        let (old_start, old_len, new_start, new_len, len) =
            (get()?, get()?, get()?, get()?, get()?);
        let mut lines = Vec::with_capacity(len.min(bytes.len()));
        let (mut o, mut n) = (old_start, new_start);
        for _ in 0..len {
            let flags = get()?;
            let kind = match flags & 3 {
                0 => LineKind::Context,
                1 => LineKind::Removed,
                2 => LineKind::Added,
                _ => return None,
            };
            let unchanged = match (flags >> 2) & 3 {
                0 => Unchanged::No,
                1 => Unchanged::Layout,
                2 => Unchanged::Moved,
                _ => return None,
            };
            let ranges = get()?;
            let mut emphasis = Vec::with_capacity(ranges.min(bytes.len()));
            let mut at = 0usize;
            for _ in 0..ranges {
                let start = at.checked_add(get()?)?;
                at = start.checked_add(get()?)?;
                emphasis.push(start..at);
            }
            lines.push(Line {
                kind,
                old: o,
                new: n,
                emphasis,
                unchanged,
                block_start: flags & 16 != 0,
                paired: flags & 32 != 0,
            });
            match kind {
                LineKind::Context => (o, n) = (o + 1, n + 1),
                LineKind::Removed => o += 1,
                LineKind::Added => n += 1,
            }
        }
        if (o - old_start, n - new_start) != (old_len, new_len) {
            return None;
        }
        hunks.push(Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
            lines,
        });
    }
    input.is_empty().then_some(Structural {
        hunks,
        regions,
        complete: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{Options, Text, hunks};

    #[test]
    fn encoding_round_trips() {
        let old =
            Text::new(b"int a(void) { return 1; }\nint b;\nint c(void) { return 2; }\n".to_vec());
        let new =
            Text::new(b"int b;\nint a(void) { return 1; }\nint c(void) { return 3; }\n".to_vec());
        let line = hunks(&old, &new, &Options::default());
        let grammars = crate::highlight::Registry::builtin();
        let s = super::super::diff(&grammars, b"a.c", &old, &new, &line).expect("diff");
        let back = decode(&encode(&s)).expect("decodes");
        assert_eq!(back.hunks, s.hunks);
        assert_eq!(back.regions, s.regions);
        assert!(decode(&encode(&s)[1..]).is_none());
    }
}

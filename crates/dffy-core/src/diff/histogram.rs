//! Histogram diff, following git's `xhistogram.c`: find the longest common
//! region seeded by the rarest lines of the old side, split around it, and
//! repeat on both halves. When every common line is too frequent to seed a
//! region, the range goes to Myers, as git does.

use super::myers;

/// Lines occurring more often than this in the old range can't seed a region
/// (git's `max_chain_length`).
const MAX_CHAIN_LEN: u32 = 64;

const NONE: u32 = u32::MAX;

pub(super) fn diff(
    before: &[u32],
    after: &[u32],
    tokens: usize,
    removed: &mut [bool],
    added: &mut [bool],
) {
    let mut h = Histogram {
        before,
        after,
        count: vec![0; tokens],
        first: vec![NONE; tokens],
        next: vec![NONE; before.len()],
    };
    let mut stack = vec![(0..before.len(), 0..after.len())];
    while let Some((a, b)) = stack.pop() {
        if a.is_empty() {
            added[b].fill(true);
            continue;
        }
        if b.is_empty() {
            removed[a].fill(true);
            continue;
        }
        match h.find_lcs(a.clone(), b.clone()) {
            Lcs::Fallback => myers::diff(
                &before[a.clone()],
                &after[b.clone()],
                &mut removed[a],
                &mut added[b],
            ),
            Lcs::None => {
                removed[a].fill(true);
                added[b].fill(true);
            }
            Lcs::Found { a: la, b: lb } => {
                stack.push((a.start..la.start, b.start..lb.start));
                stack.push((la.end..a.end, lb.end..b.end));
            }
        }
    }
}

enum Lcs {
    /// Common lines exist, but all occur too often in the old range.
    Fallback,
    /// No line is common to both ranges.
    None,
    Found {
        a: std::ops::Range<usize>,
        b: std::ops::Range<usize>,
    },
}

struct Histogram<'a> {
    before: &'a [u32],
    after: &'a [u32],
    /// Occurrences of each token in the old range being searched.
    count: Vec<u32>,
    /// First position of each token in that range; `next` chains the rest in
    /// ascending order. Both are cleared after each search.
    first: Vec<u32>,
    next: Vec<u32>,
}

impl Histogram<'_> {
    fn find_lcs(&mut self, a: std::ops::Range<usize>, b: std::ops::Range<usize>) -> Lcs {
        let (before, after) = (self.before, self.after);
        for i in a.clone().rev() {
            let t = before[i] as usize;
            self.next[i] = self.first[t];
            self.first[t] = pos(i);
            self.count[t] = self.count[t].saturating_add(1);
        }

        let mut best: Option<(usize, usize, usize)> = None;
        let mut best_len = 0;
        let mut best_cnt = MAX_CHAIN_LEN;
        let mut has_common = false;
        let mut bi = b.start;
        while bi < b.end {
            let t = after[bi] as usize;
            let cnt = self.count[t];
            let mut b_next = bi + 1;
            if cnt == 0 {
                bi = b_next;
                continue;
            }
            has_common = true;
            if cnt > best_cnt {
                bi = b_next;
                continue;
            }
            let mut ai = self.first[t] as usize;
            loop {
                let (mut a0, mut b0, mut rc) = (ai, bi, cnt);
                while a0 > a.start && b0 > b.start && before[a0 - 1] == after[b0 - 1] {
                    a0 -= 1;
                    b0 -= 1;
                    if rc > 1 {
                        rc = rc.min(self.count[before[a0] as usize]);
                    }
                }
                let (mut a1, mut b1) = (ai + 1, bi + 1);
                while a1 < a.end && b1 < b.end && before[a1] == after[b1] {
                    if rc > 1 {
                        rc = rc.min(self.count[before[a1] as usize]);
                    }
                    a1 += 1;
                    b1 += 1;
                }
                b_next = b_next.max(b1);
                if a1 - a0 > best_len || rc < best_cnt {
                    best = Some((a0, b0, a1 - a0));
                    best_len = a1 - a0;
                    best_cnt = rc;
                }
                let mut np = self.next[ai];
                while np != NONE && (np as usize) < a1 {
                    np = self.next[np as usize];
                }
                if np == NONE {
                    break;
                }
                ai = np as usize;
            }
            bi = b_next;
        }

        for i in a {
            let t = before[i] as usize;
            self.count[t] = 0;
            self.first[t] = NONE;
        }

        match best {
            Some((a0, b0, len)) => Lcs::Found {
                a: a0..a0 + len,
                b: b0..b0 + len,
            },
            None if has_common => Lcs::Fallback,
            None => Lcs::None,
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "positions index a side's lines, and 2^32 lines take at least 4 GiB of text; sides read from git stop at MAX_DIFF_BYTES (64 MiB), other callers aren't capped"
)]
fn pos(i: usize) -> u32 {
    u32::try_from(i).expect("fewer than 2^32 lines")
}

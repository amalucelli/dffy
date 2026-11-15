//! Linear-space Myers diff (divide and conquer on the middle snake), with
//! git's cost cutoff: once a split search has spent more than about sqrt(N)
//! edit steps, it splits at the furthest-reaching point instead of the
//! optimal one. That bounds the worst case on unrelated inputs at the price
//! of a non-minimal diff there, exactly as `xdl_split` does.
//!
//! The search is a direct port of `xdl_split` and keeps its signed
//! arithmetic: frontier values at the box edges can step one past the box,
//! and the comparisons rely on that.

/// Git's `XDL_MAX_COST_MIN`.
const MIN_COST_LIMIT: usize = 256;

pub(super) fn diff(a: &[u32], b: &[u32], removed: &mut [bool], added: &mut [bool]) {
    let cost_limit = (a.len() + b.len() + 3).isqrt().max(MIN_COST_LIMIT);
    diff_with_limit(a, b, removed, added, cost_limit);
}

pub(super) fn diff_with_limit(
    a: &[u32],
    b: &[u32],
    removed: &mut [bool],
    added: &mut [bool],
    cost_limit: usize,
) {
    let diagonals = a.len() + b.len() + 3;
    let mut search = Search {
        a,
        b,
        fwd: vec![0; diagonals],
        bwd: vec![0; diagonals],
        koff: signed(b.len() + 1),
        cost_limit: signed(cost_limit),
    };
    let mut stack = vec![Box {
        a0: 0,
        a1: a.len(),
        b0: 0,
        b1: b.len(),
        need_min: false,
    }];
    while let Some(mut bx) = stack.pop() {
        while bx.a0 < bx.a1 && bx.b0 < bx.b1 && a[bx.a0] == b[bx.b0] {
            bx.a0 += 1;
            bx.b0 += 1;
        }
        while bx.a0 < bx.a1 && bx.b0 < bx.b1 && a[bx.a1 - 1] == b[bx.b1 - 1] {
            bx.a1 -= 1;
            bx.b1 -= 1;
        }
        if bx.a0 == bx.a1 {
            added[bx.b0..bx.b1].fill(true);
            continue;
        }
        if bx.b0 == bx.b1 {
            removed[bx.a0..bx.a1].fill(true);
            continue;
        }
        let s = search.split(&bx);
        debug_assert!(
            (s.x, s.y) != (bx.a0, bx.b0) && (s.x, s.y) != (bx.a1, bx.b1),
            "split must make progress"
        );
        stack.push(Box {
            a0: bx.a0,
            a1: s.x,
            b0: bx.b0,
            b1: s.y,
            need_min: s.min_lo,
        });
        stack.push(Box {
            a0: s.x,
            a1: bx.a1,
            b0: s.y,
            b1: bx.b1,
            need_min: s.min_hi,
        });
    }
}

#[expect(
    clippy::expect_used,
    reason = "only lengths of u32 slices (plus one), positions within them and a square root of their sum come here, and such a slice holds at most isize::MAX / 4 items"
)]
fn signed(u: usize) -> isize {
    isize::try_from(u).expect("slice lengths fit in isize")
}

/// Positions handed to slices are always inside the box; a negative one is a
/// bug and should fail loudly at the index.
fn unsigned(i: isize) -> usize {
    usize::try_from(i).unwrap_or(usize::MAX)
}

struct Box {
    a0: usize,
    a1: usize,
    b0: usize,
    b1: usize,
    /// Skip the cost cutoff in this box. Set on the half of a cutoff split
    /// whose path was found optimally, as git does.
    need_min: bool,
}

struct Split {
    x: usize,
    y: usize,
    min_lo: bool,
    min_hi: bool,
}

/// Frontiers are indexed by diagonal `d = x - y` shifted by `koff`, so every
/// diagonal of the outermost box lands at a valid index.
struct Search<'a> {
    a: &'a [u32],
    b: &'a [u32],
    /// Furthest x reached per diagonal by the forward search.
    fwd: Vec<isize>,
    /// Smallest x reached per diagonal by the backward search.
    bwd: Vec<isize>,
    koff: isize,
    cost_limit: isize,
}

impl Search<'_> {
    fn snake_fwd(&self, mut x: isize, mut y: isize, lim1: isize, lim2: isize) -> isize {
        while x < lim1 && y < lim2 && self.a[unsigned(x)] == self.b[unsigned(y)] {
            x += 1;
            y += 1;
        }
        x
    }

    fn snake_bwd(&self, mut x: isize, mut y: isize, off1: isize, off2: isize) -> isize {
        while x > off1 && y > off2 && self.a[unsigned(x - 1)] == self.b[unsigned(y - 1)] {
            x -= 1;
            y -= 1;
        }
        x
    }

    /// Finds the middle snake of a box whose ends don't match (the caller
    /// trimmed them) and returns the point to split at.
    fn split(&mut self, bx: &Box) -> Split {
        let (off1, lim1, off2, lim2) = (signed(bx.a0), signed(bx.a1), signed(bx.b0), signed(bx.b1));
        let koff = self.koff;
        let k = move |d: isize| unsigned(d + koff);
        let diag_lo = off1 - lim2;
        let diag_hi = lim1 - off2;
        let fwd_mid = off1 - off2;
        let bwd_mid = lim1 - lim2;
        let odd = (fwd_mid - bwd_mid) & 1 != 0;
        let (mut fwd_lo, mut fwd_hi, mut bwd_lo, mut bwd_hi) = (fwd_mid, fwd_mid, bwd_mid, bwd_mid);
        self.fwd[k(fwd_mid)] = off1;
        self.bwd[k(bwd_mid)] = lim1;

        let mut cost = 1;
        loop {
            // Widen the diagonal range by one, or shrink it where it hits the
            // box edge. Sentinels outside the range keep the neighbour
            // comparisons below branch-free.
            if fwd_lo > diag_lo {
                fwd_lo -= 1;
                self.fwd[k(fwd_lo - 1)] = -1;
            } else {
                fwd_lo += 1;
            }
            if fwd_hi < diag_hi {
                fwd_hi += 1;
                self.fwd[k(fwd_hi + 1)] = -1;
            } else {
                fwd_hi -= 1;
            }
            let mut d = fwd_hi;
            while d >= fwd_lo {
                let x = if self.fwd[k(d - 1)] >= self.fwd[k(d + 1)] {
                    self.fwd[k(d - 1)] + 1
                } else {
                    self.fwd[k(d + 1)]
                };
                let x = self.snake_fwd(x, x - d, lim1, lim2);
                self.fwd[k(d)] = x;
                if odd && bwd_lo <= d && d <= bwd_hi && self.bwd[k(d)] <= x {
                    return Split {
                        x: unsigned(x),
                        y: unsigned(x - d),
                        min_lo: true,
                        min_hi: true,
                    };
                }
                d -= 2;
            }

            if bwd_lo > diag_lo {
                bwd_lo -= 1;
                self.bwd[k(bwd_lo - 1)] = isize::MAX;
            } else {
                bwd_lo += 1;
            }
            if bwd_hi < diag_hi {
                bwd_hi += 1;
                self.bwd[k(bwd_hi + 1)] = isize::MAX;
            } else {
                bwd_hi -= 1;
            }
            let mut d = bwd_hi;
            while d >= bwd_lo {
                let x = if self.bwd[k(d - 1)] < self.bwd[k(d + 1)] {
                    self.bwd[k(d - 1)]
                } else {
                    self.bwd[k(d + 1)] - 1
                };
                let x = self.snake_bwd(x, x - d, off1, off2);
                self.bwd[k(d)] = x;
                if !odd && fwd_lo <= d && d <= fwd_hi && x <= self.fwd[k(d)] {
                    return Split {
                        x: unsigned(x),
                        y: unsigned(x - d),
                        min_lo: true,
                        min_hi: true,
                    };
                }
                d -= 2;
            }

            if !bx.need_min && cost >= self.cost_limit {
                return self.cutoff((off1, lim1, off2, lim2), (fwd_lo, fwd_hi), (bwd_lo, bwd_hi));
            }
            cost += 1;
        }
    }

    /// Splits at whichever of the forward or backward frontiers got further
    /// into the box.
    fn cutoff(
        &self,
        (off1, lim1, off2, lim2): (isize, isize, isize, isize),
        (fwd_lo, fwd_hi): (isize, isize),
        (bwd_lo, bwd_hi): (isize, isize),
    ) -> Split {
        let koff = self.koff;
        let k = move |d: isize| unsigned(d + koff);

        let (mut fwd_best, mut fwd_best_x) = (-1, -1);
        let mut d = fwd_hi;
        while d >= fwd_lo {
            let mut x = self.fwd[k(d)].min(lim1);
            let mut y = x - d;
            if lim2 < y {
                x = lim2 + d;
                y = lim2;
            }
            if fwd_best < x + y {
                fwd_best = x + y;
                fwd_best_x = x;
            }
            d -= 2;
        }

        let (mut bwd_best, mut bwd_best_x) = (isize::MAX, isize::MAX);
        let mut d = bwd_hi;
        while d >= bwd_lo {
            let mut x = self.bwd[k(d)].max(off1);
            let mut y = x - d;
            if y < off2 {
                x = off2 + d;
                y = off2;
            }
            if x + y < bwd_best {
                bwd_best = x + y;
                bwd_best_x = x;
            }
            d -= 2;
        }

        if (lim1 + lim2) - bwd_best < fwd_best - (off1 + off2) {
            Split {
                x: unsigned(fwd_best_x),
                y: unsigned(fwd_best - fwd_best_x),
                min_lo: true,
                min_hi: false,
            }
        } else {
            Split {
                x: unsigned(bwd_best_x),
                y: unsigned(bwd_best - bwd_best_x),
                min_lo: false,
                min_hi: true,
            }
        }
    }
}

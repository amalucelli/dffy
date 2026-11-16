//! The flowchart's layered layout, in the manner of Sugiyama:
//!
//! 1. Cycles are broken by turning the back edges of a depth-first search
//!    around, then each node's layer is its longest path from a source.
//! 2. An edge spanning several layers passes through a placeholder in each
//!    layer between, so every edge segment joins adjacent layers.
//! 3. Nodes are ordered within layers by the mean position of their
//!    neighbors, sweeping down and up and keeping the order with the
//!    fewest crossings; a subgraph's nodes stay together.
//! 4. Each layer is placed as close to its neighbors as its gaps allow,
//!    and subgraph frames push other nodes out.
//! 5. Between two layers, an edge that bends runs across on a track of its
//!    own, so edges never share a stretch of line.
//!
//! Positions are `(u, v)`: `u` runs from layer to layer and `v` along a
//! layer. Top-down charts have `u` down and `v` right, left-right ones the
//! other way round; bottom-up and right-left charts are those flipped.

use std::collections::HashMap;

use super::{Direction, Graph, Head, Shape};
use crate::canvas::{
    self, BORDER, Canvas, DOWN, EDGE, EDGE_LABEL, LEFT, PLAIN, RIGHT, Stroke, TITLE, UP,
};
use crate::{Line, Span, wrap};

/// Past this many nodes and placeholders the source shows instead.
const MAX_ITEMS: usize = 3000;
/// Ordering sweeps, alternating down and up.
const SWEEPS: usize = 8;
/// Placement passes, each one down and one up.
const PASSES: usize = 4;
/// Rounds of pushing nodes out of frames before a frame that still has a
/// stranger inside is dropped.
const REPAIRS: usize = 8;
/// Fixed-point scale of the ordering keys.
const SCALE: i64 = 1024;

/// A node or a placeholder an edge passes through.
#[derive(Debug, Clone)]
struct Item {
    node: Option<usize>,
    layer: usize,
    group: Option<usize>,
    len_u: i64,
    len_v: i64,
    u: i64,
    v: i64,
}

/// An edge's stretch between two adjacent layers.
#[derive(Debug, Clone)]
struct Seg {
    edge: usize,
    upper: usize,
    lower: usize,
    /// Where it leaves the upper item and enters the lower one, along `v`.
    src: i64,
    dst: i64,
    /// The track it runs across on, when it bends.
    track: Option<i64>,
    /// For a top-down chart, the label's row among the channel's label
    /// rows and its first column.
    label: Option<(i64, i64)>,
}

/// A subgraph's frame: the layers it spans, its sides along `v`, and how
/// many frames start (`top`) or end (`bottom`) at its boundary inside it,
/// itself included.
#[derive(Debug, Clone, Copy)]
struct Frame {
    first: usize,
    last: usize,
    lo: i64,
    hi: i64,
    top: i64,
    bottom: i64,
}

pub(super) struct Layout<'g> {
    graph: &'g Graph,
    vertical: bool,
    flipped: bool,
    gap: i64,
    /// Line between a layer and the first track of the channel after it.
    stub: i64,
    labels: Vec<Vec<String>>,
    reversed: Vec<bool>,
    items: Vec<Item>,
    segs: Vec<Seg>,
    /// Each edge's segments, from its upper end.
    chains: Vec<Vec<usize>>,
    order: Vec<Vec<usize>>,
    /// By subgraph; `None` for one not drawn as a frame.
    frames: Vec<Option<Frame>>,
    /// Per layer: the tracks, label rows (or columns), frame tops starting
    /// at it, frame bottoms ending at it, and padding of the channel after
    /// it.
    tracks: Vec<i64>,
    label_len: Vec<i64>,
    tops: Vec<i64>,
    bottoms: Vec<i64>,
    extra: Vec<i64>,
    layer_len: Vec<i64>,
    layer_u: Vec<i64>,
    total_u: i64,
    min_v: i64,
}

fn int(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn cell(n: i64) -> usize {
    usize::try_from(n).unwrap_or(0)
}

/// `a / b` rounded to the nearest integer; `b` is positive.
fn div_round(a: i64, b: i64) -> i64 {
    (2 * a + b).div_euclid(2 * b)
}

impl<'g> Layout<'g> {
    /// Lays `graph` out with node labels wrapped at `wrap_at` columns and
    /// `gap` columns between the nodes of a top-down layer. `None` past
    /// [`MAX_ITEMS`].
    pub(super) fn new(graph: &'g Graph, wrap_at: usize, gap: i64) -> Option<Self> {
        let vertical = matches!(graph.direction, Direction::Down | Direction::Up);
        let mut labels: Vec<Vec<String>> = graph
            .nodes
            .iter()
            .map(|node| wrap(&node.label, wrap_at))
            .collect();
        for edge in &graph.edges {
            if edge.from == edge.to && edge.stroke.is_some() {
                labels[edge.from].push(
                    edge.label
                        .as_ref()
                        .map_or_else(|| "↻".to_owned(), |l| format!("↻ {l}")),
                );
            }
        }
        let reversed = back_edges(graph);
        let layer = layers(graph, &reversed);
        let mut layout = Self {
            graph,
            vertical,
            flipped: matches!(graph.direction, Direction::Up | Direction::Left),
            gap: if vertical { gap } else { 1 },
            stub: if vertical { 1 } else { 2 },
            labels,
            reversed,
            items: Vec::new(),
            segs: Vec::new(),
            chains: vec![Vec::new(); graph.edges.len()],
            order: Vec::new(),
            frames: Vec::new(),
            tracks: Vec::new(),
            label_len: Vec::new(),
            tops: Vec::new(),
            bottoms: Vec::new(),
            extra: Vec::new(),
            layer_len: Vec::new(),
            layer_u: Vec::new(),
            total_u: 0,
            min_v: 0,
        };
        layout.build(&layer)?;
        layout.measure();
        layout.reorder();
        layout.position();
        layout.frame();
        layout.ports();
        layout.channels();
        layout.rows();
        layout.min_v = layout.lowest_v();
        Some(layout)
    }

    /// The items and segments: a placeholder for each layer an edge passes.
    fn build(&mut self, layer: &[usize]) -> Option<()> {
        let graph = self.graph;
        self.items = graph
            .nodes
            .iter()
            .zip(layer)
            .enumerate()
            .map(|(i, (node, &layer))| Item {
                node: Some(i),
                layer,
                group: node.group,
                len_u: 1,
                len_v: 1,
                u: 0,
                v: 0,
            })
            .collect();
        for (e, edge) in graph.edges.iter().enumerate() {
            if edge.from == edge.to || edge.stroke.is_none() {
                continue;
            }
            let (a, b) = if self.reversed[e] {
                (edge.to, edge.from)
            } else {
                (edge.from, edge.to)
            };
            let group = graph.common(graph.nodes[a].group, graph.nodes[b].group);
            let mut upper = a;
            for l in layer[a] + 1..=layer[b] {
                let lower = if l == layer[b] {
                    b
                } else {
                    self.items.push(Item {
                        node: None,
                        layer: l,
                        group,
                        len_u: 1,
                        len_v: 1,
                        u: 0,
                        v: 0,
                    });
                    self.items.len() - 1
                };
                self.chains[e].push(self.segs.len());
                self.segs.push(Seg {
                    edge: e,
                    upper,
                    lower,
                    src: 0,
                    dst: 0,
                    track: None,
                    label: None,
                });
                upper = lower;
            }
            if self.items.len() > MAX_ITEMS {
                return None;
            }
        }
        let depth = layer.iter().max().map_or(1, |l| l + 1);
        self.order = vec![Vec::new(); depth];
        for (i, item) in self.items.iter().enumerate() {
            self.order[item.layer].push(i);
        }
        Some(())
    }

    /// Node boxes: their label plus a border and a space each side, and
    /// along `v` room for a port per edge on each side.
    fn measure(&mut self) {
        let mut outs = vec![0; self.items.len()];
        let mut ins = vec![0; self.items.len()];
        for seg in &self.segs {
            outs[seg.upper] += 1;
            ins[seg.lower] += 1;
        }
        for (i, item) in self.items.iter_mut().enumerate() {
            let Some(node) = item.node else { continue };
            let lines = &self.labels[node];
            let w = int(lines.iter().map(|l| canvas::width(l)).max().unwrap_or(0)) + 4;
            let h = int(lines.len()) + 2;
            let (len_u, len_v) = if self.vertical { (h, w) } else { (w, h) };
            item.len_u = len_u;
            item.len_v = len_v.max(outs[i].max(ins[i]) + 2);
        }
    }

    fn neighbors(&self) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
        let mut ups = vec![Vec::new(); self.items.len()];
        let mut downs = vec![Vec::new(); self.items.len()];
        for seg in &self.segs {
            downs[seg.upper].push(seg.lower);
            ups[seg.lower].push(seg.upper);
        }
        (ups, downs)
    }

    fn reorder(&mut self) {
        let n = self.items.len();
        let chains: Vec<Vec<usize>> = self
            .items
            .iter()
            .map(|item| self.graph.ancestry(item.group))
            .collect();
        let (ups, downs) = self.neighbors();
        let mut pos = vec![0; n];
        let mut key = vec![0; n];
        for row in &mut self.order {
            for (p, &i) in row.iter().enumerate() {
                key[i] = int(p) * SCALE;
            }
            *row = arrange(row, &key, 0, &chains);
            for (p, &i) in row.iter().enumerate() {
                pos[i] = int(p);
            }
        }
        let depth = self.order.len();
        let mut best = (self.crossings(&pos), self.order.clone());
        for round in 0..SWEEPS {
            let down = round % 2 == 0;
            for step in 0..depth.saturating_sub(1) {
                let (k, near) = if down {
                    (step + 1, &ups)
                } else {
                    (depth - 2 - step, &downs)
                };
                for &i in &self.order[k] {
                    key[i] = if near[i].is_empty() {
                        pos[i] * SCALE
                    } else {
                        near[i].iter().map(|&j| pos[j]).sum::<i64>() * SCALE / int(near[i].len())
                    };
                }
                let row = arrange(&self.order[k], &key, 0, &chains);
                for (p, &i) in row.iter().enumerate() {
                    pos[i] = int(p);
                }
                self.order[k] = row;
            }
            let crossings = self.crossings(&pos);
            if crossings < best.0 {
                best = (crossings, self.order.clone());
            }
        }
        self.order = best.1;
    }

    /// How many pairs of segments cross, with items at positions `pos`.
    fn crossings(&self, pos: &[i64]) -> usize {
        let mut channels = vec![Vec::new(); self.order.len()];
        for seg in &self.segs {
            channels[self.items[seg.upper].layer].push((pos[seg.upper], pos[seg.lower]));
        }
        channels
            .into_iter()
            .map(|mut channel| {
                channel.sort_unstable();
                let mut lower: Vec<i64> = channel.into_iter().map(|(_, l)| l).collect();
                inversions(&mut lower)
            })
            .sum()
    }

    /// Room between neighbors `a` and `b` of a layer: the gap, and the
    /// border and margin of each frame that ends between them.
    fn spacing(&self, a: usize, b: usize) -> i64 {
        let (ga, gb) = (self.items[a].group, self.items[b].group);
        let common = self.graph.depth(self.graph.common(ga, gb));
        let frames = self.graph.depth(ga) + self.graph.depth(gb) - 2 * common;
        self.gap + 2 * int(frames)
    }

    fn position(&mut self) {
        for k in 0..self.order.len() {
            self.place(k, &vec![0; self.order[k].len()]);
        }
        let (ups, downs) = self.neighbors();
        let depth = self.order.len();
        for _ in 0..PASSES {
            for k in 1..depth {
                self.align(k, &ups);
            }
            for k in (0..depth.saturating_sub(1)).rev() {
                self.align(k, &downs);
            }
        }
    }

    /// Places layer `k` centered on its neighbors in `near`.
    fn align(&mut self, k: usize, near: &[Vec<usize>]) {
        let desired: Vec<i64> = self.order[k]
            .iter()
            .map(|&i| {
                let item = &self.items[i];
                if near[i].is_empty() {
                    return item.v;
                }
                let m = int(near[i].len());
                let sum: i64 = near[i]
                    .iter()
                    .map(|&j| self.items[j].v + self.items[j].len_v / 2)
                    .sum();
                div_round(sum - m * (item.len_v / 2), m)
            })
            .collect();
        self.place(k, &desired);
    }

    /// Places layer `k`'s items as near the `desired` positions as their
    /// order and spacing allow, least squares: pooling adjacent violators.
    fn place(&mut self, k: usize, desired: &[i64]) {
        let row = &self.order[k];
        let mut offsets = Vec::with_capacity(row.len());
        let mut at = 0;
        for (p, &i) in row.iter().enumerate() {
            if p > 0 {
                at += self.items[row[p - 1]].len_v + self.spacing(row[p - 1], i);
            }
            offsets.push(at);
        }
        let mut blocks: Vec<(i64, i64)> = Vec::new();
        for (p, &d) in desired.iter().enumerate() {
            let mut block = (d - offsets[p], 1);
            while let Some(&(sum, count)) = blocks.last() {
                if sum * block.1 < block.0 * count {
                    break;
                }
                block = (sum + block.0, count + block.1);
                blocks.pop();
            }
            blocks.push(block);
        }
        let mut p = 0;
        for (sum, count) in blocks {
            let y = div_round(sum, count);
            for _ in 0..count {
                self.items[row[p]].v = y + offsets[p];
                p += 1;
            }
        }
    }

    /// Frames for the subgraphs, with other nodes pushed out of them; a
    /// frame that still holds a stranger, or crosses another frame, is
    /// dropped and listed under the drawing instead.
    fn frame(&mut self) {
        let mut inner_first: Vec<usize> = (0..self.graph.groups.len()).collect();
        inner_first.sort_by_key(|&g| std::cmp::Reverse(self.graph.depth(Some(g))));
        for _ in 0..REPAIRS {
            self.frames = self.bounds(&inner_first);
            if !self.repair() {
                break;
            }
        }
        self.frames = self.bounds(&inner_first);
        self.validate();
        for &g in &inner_first {
            let Some(mut frame) = self.frames[g] else {
                continue;
            };
            let children = (0..self.frames.len())
                .filter(|&c| self.graph.groups[c].parent == Some(g))
                .filter_map(|c| self.frames[c]);
            for child in children {
                if child.first == frame.first {
                    frame.top = frame.top.max(child.top + 1);
                }
                if child.last == frame.last {
                    frame.bottom = frame.bottom.max(child.bottom + 1);
                }
            }
            self.frames[g] = Some(frame);
        }
    }

    /// Each subgraph's frame around its items and the frames inside it.
    fn bounds(&self, inner_first: &[usize]) -> Vec<Option<Frame>> {
        let groups = &self.graph.groups;
        let mut frames: Vec<Option<Frame>> = vec![None; groups.len()];
        for &g in inner_first {
            let mut frame: Option<Frame> = None;
            let mut extend = |first: usize, last: usize, lo: i64, hi: i64| {
                frame = Some(match frame {
                    None => Frame {
                        first,
                        last,
                        lo,
                        hi,
                        top: 1,
                        bottom: 1,
                    },
                    Some(f) => Frame {
                        first: f.first.min(first),
                        last: f.last.max(last),
                        lo: f.lo.min(lo),
                        hi: f.hi.max(hi),
                        ..f
                    },
                });
            };
            for item in self.items.iter().filter(|item| item.group == Some(g)) {
                extend(item.layer, item.layer, item.v - 2, item.v + item.len_v + 1);
            }
            for (c, child) in frames.iter().enumerate() {
                if let Some(child) = child
                    && groups[c].parent == Some(g)
                {
                    extend(child.first, child.last, child.lo - 2, child.hi + 2);
                }
            }
            if let Some(f) = &mut frame
                && self.vertical
            {
                f.hi = f.hi.max(f.lo + int(canvas::width(&groups[g].title)) + 5);
            }
            frames[g] = frame;
        }
        frames
    }

    /// Pushes items outside a frame they don't belong to out of it, with
    /// their neighbors; whether anything moved.
    fn repair(&mut self) -> bool {
        let mut moved = false;
        for g in 0..self.frames.len() {
            let Some(frame) = self.frames[g] else {
                continue;
            };
            for k in frame.first..=frame.last {
                let row = self.order[k].clone();
                let inside: Vec<bool> = row
                    .iter()
                    .map(|&i| self.graph.contains(g, self.items[i].group))
                    .collect();
                let first_inside = inside.iter().position(|&b| b);
                for (p, &i) in row.iter().enumerate() {
                    let item = &self.items[i];
                    if inside[p] {
                        continue;
                    }
                    let left = first_inside
                        .map_or(2 * item.v + item.len_v < frame.lo + frame.hi, |first| {
                            p < first
                        });
                    if left && item.v + item.len_v > frame.lo - 1 {
                        let v = frame.lo - 1 - item.len_v;
                        self.shove(&row, p, v, true);
                        moved = true;
                    } else if !left && item.v < frame.hi + 2 {
                        self.shove(&row, p, frame.hi + 2, false);
                        moved = true;
                    }
                }
            }
        }
        moved | self.separate()
    }

    /// Moves the right one of two overlapping frames, neither inside the
    /// other, clear of the left one, with everything right of it.
    fn separate(&mut self) -> bool {
        let n = self.frames.len();
        for g in 0..n {
            for h in 0..n {
                let (Some(a), Some(b)) = (self.frames[g], self.frames[h]) else {
                    continue;
                };
                let apart = self.graph.contains(g, Some(h)) || self.graph.contains(h, Some(g));
                let overlap =
                    a.first <= b.last && b.first <= a.last && a.lo <= b.hi && b.lo <= a.hi;
                if apart
                    || !overlap
                    || a.lo + a.hi > b.lo + b.hi
                    || (a.lo + a.hi == b.lo + b.hi && g > h)
                {
                    continue;
                }
                let shift = a.hi + 2 - b.lo;
                for k in b.first..=b.last {
                    let row = self.order[k].clone();
                    let inside: Vec<usize> = (0..row.len())
                        .filter(|&p| self.graph.contains(h, self.items[row[p]].group))
                        .collect();
                    let Some(&last) = inside.last() else { continue };
                    for &p in &inside {
                        self.items[row[p]].v += shift;
                    }
                    self.shove(&row, last, self.items[row[last]].v, false);
                }
                return true;
            }
        }
        false
    }

    /// Moves the item at `p` of `row` to `v`, and the ones beyond it in the
    /// direction it moved as far as their spacing needs.
    fn shove(&mut self, row: &[usize], p: usize, v: i64, leftward: bool) {
        self.items[row[p]].v = v;
        if leftward {
            for q in (0..p).rev() {
                let limit = self.items[row[q + 1]].v
                    - self.spacing(row[q], row[q + 1])
                    - self.items[row[q]].len_v;
                if self.items[row[q]].v <= limit {
                    break;
                }
                self.items[row[q]].v = limit;
            }
        } else {
            for q in p + 1..row.len() {
                let limit = self.items[row[q - 1]].v
                    + self.items[row[q - 1]].len_v
                    + self.spacing(row[q - 1], row[q]);
                if self.items[row[q]].v >= limit {
                    break;
                }
                self.items[row[q]].v = limit;
            }
        }
    }

    fn validate(&mut self) {
        for g in 0..self.frames.len() {
            let Some(frame) = self.frames[g] else {
                continue;
            };
            let stranger = (frame.first..=frame.last)
                .flat_map(|k| &self.order[k])
                .any(|&i| {
                    let item = &self.items[i];
                    !self.graph.contains(g, item.group)
                        && item.v <= frame.hi
                        && item.v + item.len_v > frame.lo
                });
            let crossed = (0..g).any(|h| {
                self.frames[h].is_some_and(|o| {
                    !self.graph.contains(g, Some(h))
                        && !self.graph.contains(h, Some(g))
                        && o.first <= frame.last
                        && frame.first <= o.last
                        && o.lo <= frame.hi
                        && frame.lo <= o.hi
                })
            });
            if stranger || crossed {
                self.frames[g] = None;
            }
        }
    }

    /// Where each segment leaves and enters its items: spread along the
    /// item's side in the order of the items at their other ends.
    fn ports(&mut self) {
        let mut outs = vec![Vec::new(); self.items.len()];
        let mut ins = vec![Vec::new(); self.items.len()];
        for (s, seg) in self.segs.iter().enumerate() {
            outs[seg.upper].push(s);
            ins[seg.lower].push(s);
        }
        let center = |item: &Item| 2 * item.v + item.len_v;
        for (i, (outs, ins)) in outs.iter().zip(&ins).enumerate() {
            let (v, len) = (self.items[i].v, self.items[i].len_v);
            let mut out: Vec<(i64, usize)> = outs
                .iter()
                .map(|&s| (center(&self.items[self.segs[s].lower]), s))
                .collect();
            out.sort_unstable();
            for (k, &(_, s)) in out.iter().enumerate() {
                self.segs[s].src = port(v, len, k, out.len());
            }
            let mut inn: Vec<(i64, usize)> = ins
                .iter()
                .map(|&s| (center(&self.items[self.segs[s].upper]), s))
                .collect();
            inn.sort_unstable();
            for (k, &(_, s)) in inn.iter().enumerate() {
                self.segs[s].dst = port(v, len, k, inn.len());
            }
        }
    }

    /// The label of segment `s`, carried by each edge's first segment.
    fn label(&self, s: usize) -> Option<&str> {
        let edge = self.segs[s].edge;
        if self.chains[edge].first() != Some(&s) {
            return None;
        }
        self.graph.edges[edge].label.as_deref()
    }

    fn channels(&mut self) {
        let depth = self.order.len();
        let mut by_layer = vec![Vec::new(); depth];
        for (s, seg) in self.segs.iter().enumerate() {
            by_layer[self.items[seg.upper].layer].push(s);
        }
        self.tracks = vec![0; depth];
        self.label_len = vec![0; depth];
        for (k, channel) in by_layer.iter().enumerate() {
            self.tracks[k] = self.assign_tracks(channel);
            // Rounded up, so a label edited a little longer or shorter
            // doesn't move every column after it.
            self.label_len[k] = if self.vertical {
                self.place_labels(k, channel)
            } else {
                channel
                    .iter()
                    .filter_map(|&s| self.label(s))
                    .map(|l| (int(canvas::width(l)) + 5) / 4 * 4)
                    .max()
                    .unwrap_or(0)
            };
        }
    }

    /// Tracks for the bending segments of a channel, so that no two share
    /// a stretch of line: a segment leaving from the column another
    /// arrives at runs across above it, and segments on one track don't
    /// overlap. Returns the number of tracks.
    fn assign_tracks(&mut self, channel: &[usize]) -> i64 {
        let bent: Vec<usize> = channel
            .iter()
            .copied()
            .filter(|&s| self.segs[s].src != self.segs[s].dst)
            .collect();
        let arriving: HashMap<i64, usize> = bent
            .iter()
            .enumerate()
            .map(|(k, &s)| (self.segs[s].dst, k))
            .collect();
        let below: Vec<Option<usize>> = bent
            .iter()
            .map(|&s| arriving.get(&self.segs[s].src).copied())
            .collect();
        let mut above = vec![None; bent.len()];
        for (a, b) in below.iter().enumerate() {
            if let Some(b) = *b {
                above[b] = Some(a);
            }
        }
        // Ports are unique, so the constraints form paths and cycles; a
        // cycle is cut where the walk starts.
        let mut level = vec![usize::MAX; bent.len()];
        let starts = (0..bent.len())
            .filter(|&a| above[a].is_none())
            .chain(0..bent.len());
        for start in starts {
            let (mut at, mut l) = (Some(start), 0);
            while let Some(a) = at.filter(|&a| level[a] == usize::MAX) {
                level[a] = l;
                l += 1;
                at = below[a];
            }
        }
        let spans: Vec<(i64, i64)> = bent
            .iter()
            .map(|&s| {
                let seg = &self.segs[s];
                (seg.src.min(seg.dst), seg.src.max(seg.dst))
            })
            .collect();
        // Segments turning left take tracks from the left, those turning
        // right from the right, so a fan of them doesn't cross itself.
        let mut queue: Vec<usize> = (0..bent.len()).collect();
        queue.sort_by_key(|&a| {
            let seg = &self.segs[bent[a]];
            let rightward = seg.dst > seg.src;
            (
                level[a],
                rightward,
                if rightward { -seg.src } else { seg.src },
            )
        });
        let mut rows: Vec<Vec<(i64, i64)>> = Vec::new();
        let mut track = vec![0; bent.len()];
        for a in queue {
            let (lo, hi) = spans[a];
            let start = above[a]
                .filter(|&p| level[p] < level[a])
                .map_or(0, |p| track[p] + 1);
            let free = |t: &usize| {
                rows.get(*t)
                    .is_none_or(|r| r.iter().all(|&(l, h)| h < lo || hi < l))
            };
            let t = (start..=start + rows.len())
                .find(free)
                .unwrap_or(rows.len());
            if rows.len() <= t {
                rows.resize_with(t + 1, Vec::new);
            }
            rows[t].push((lo, hi));
            track[a] = t;
            self.segs[bent[a]].track = Some(int(t));
        }
        int(rows.len())
    }

    /// Places a top-down channel's labels on the segments' last stretch:
    /// on the line, or beside it when that would cover another line, in
    /// the first label row they fit. Returns the number of rows.
    ///
    /// A label never reaches left of the leftmost node or frame: that
    /// would shift the whole drawing, and an edited label would then show
    /// every line of it changed.
    fn place_labels(&mut self, k: usize, channel: &[usize]) -> i64 {
        let mut walls: Vec<i64> = channel.iter().map(|&s| self.segs[s].dst).collect();
        for frame in self.frames.iter().flatten() {
            if frame.first <= k && frame.last > k {
                walls.extend([frame.lo, frame.hi]);
            }
        }
        let floor = self
            .items
            .iter()
            .map(|item| item.v)
            .chain(self.frames.iter().flatten().map(|f| f.lo))
            .min()
            .unwrap_or(0);
        let mut rows: Vec<Vec<(i64, i64)>> = Vec::new();
        for &s in channel {
            let Some(text) = self.label(s) else { continue };
            let w = int(canvas::width(text)) + 2;
            let dst = self.segs[s].dst;
            let clear = |start: &i64| {
                *start >= floor
                    && walls
                        .iter()
                        .all(|&x| x == dst || x < *start || x >= start + w)
            };
            let start = [dst - w / 2, dst + 1, dst - w]
                .into_iter()
                .find(clear)
                .unwrap_or_else(|| (dst - w / 2).max(floor));
            let row = rows
                .iter()
                .position(|r| r.iter().all(|&(a, b)| b + 1 < start || start + w < a))
                .unwrap_or(rows.len());
            if row == rows.len() {
                rows.push(Vec::new());
            }
            rows[row].push((start, start + w - 1));
            self.segs[s].label = Some((int(row), start));
        }
        int(rows.len())
    }

    /// Positions along `u`: layers, the channels between them, and room
    /// for frame borders; frames in left-right charts widen to their
    /// titles.
    fn rows(&mut self) {
        let depth = self.order.len();
        self.layer_len = self
            .order
            .iter()
            .map(|row| {
                row.iter()
                    .filter(|&&i| self.items[i].node.is_some())
                    .map(|&i| self.items[i].len_u)
                    .max()
                    .unwrap_or(1)
            })
            .collect();
        self.tops = vec![0; depth];
        self.bottoms = vec![0; depth];
        for frame in self.frames.iter().flatten() {
            self.tops[frame.first] = self.tops[frame.first].max(frame.top);
            self.bottoms[frame.last] = self.bottoms[frame.last].max(frame.bottom);
        }
        self.extra = vec![0; depth];
        loop {
            self.stack_layers();
            if self.vertical {
                break;
            }
            let short = self.frames.iter().enumerate().find_map(|(g, frame)| {
                let frame = (*frame)?;
                let need = int(canvas::width(&self.graph.groups[g].title)) + 6;
                let have = self.bottom_row(&frame) - self.top_row(&frame) + 1;
                (have < need).then_some((frame.last, need - have))
            });
            match short {
                Some((k, more)) => self.extra[k] += more,
                None => break,
            }
        }
        for item in &mut self.items {
            let k = item.layer;
            if item.node.is_some() {
                item.u = self.layer_u[k] + (self.layer_len[k] - item.len_u) / 2;
            } else {
                item.u = self.layer_u[k];
                item.len_u = self.layer_len[k];
            }
        }
    }

    fn stack_layers(&mut self) {
        let depth = self.order.len();
        self.layer_u = vec![0; depth];
        let mut u = if self.tops[0] > 0 {
            borders(self.tops[0]) + 1
        } else {
            0
        };
        for k in 0..depth {
            self.layer_u[k] = u;
            if k + 1 < depth {
                u = self.labels_at(k) + self.label_len[k] + borders(self.tops[k + 1]) + 1;
            }
        }
        let last = depth - 1;
        self.total_u = if self.bottoms[last] > 0 {
            self.bottoms_at(last) + borders(self.bottoms[last])
        } else {
            self.layer_u[last] + self.layer_len[last]
        };
    }

    /// The first row of the channel after layer `k` for frame bottoms.
    fn bottoms_at(&self, k: usize) -> i64 {
        self.layer_u[k] + self.layer_len[k] + self.stub + self.extra[k]
    }

    fn tracks_at(&self, k: usize) -> i64 {
        self.bottoms_at(k) + borders(self.bottoms[k])
    }

    fn labels_at(&self, k: usize) -> i64 {
        self.tracks_at(k) + self.tracks[k]
    }

    /// The first row for the tops of frames starting at layer `k`.
    fn tops_at(&self, k: usize) -> i64 {
        if k == 0 {
            0
        } else {
            self.labels_at(k - 1) + self.label_len[k - 1]
        }
    }

    fn top_row(&self, frame: &Frame) -> i64 {
        self.tops_at(frame.first) + 2 * (self.tops[frame.first] - frame.top)
    }

    fn bottom_row(&self, frame: &Frame) -> i64 {
        self.bottoms_at(frame.last) + 2 * (frame.bottom - 1)
    }

    /// The cell at `(u, v)` on the canvas.
    fn at(&self, u: i64, v: i64) -> (usize, usize) {
        let u = if self.flipped {
            self.total_u - 1 - u
        } else {
            u
        };
        let v = v - self.min_v;
        if self.vertical {
            (cell(v), cell(u))
        } else {
            (cell(u), cell(v))
        }
    }

    /// The box from `(u0, v0)` to `(u1, v1)`, as canvas corners.
    fn corners(
        &self,
        (u0, v0): (i64, i64),
        (u1, v1): (i64, i64),
    ) -> ((usize, usize), (usize, usize)) {
        let (a, b) = (self.at(u0, v0), self.at(u1, v1));
        ((a.0.min(b.0), a.1.min(b.1)), (a.0.max(b.0), a.1.max(b.1)))
    }

    /// The drawing's width and height, without drawing it.
    pub(super) fn size(&self) -> (usize, usize) {
        let items = self.items.iter().map(|item| item.v + item.len_v - 1);
        let frames = self.frames.iter().flatten().map(|f| f.hi);
        let labels = self.segs.iter().enumerate().filter_map(|(s, seg)| {
            let (_, v) = seg.label?;
            Some(v + int(canvas::width(self.label(s)?)) + 1)
        });
        let span_v = cell(items.chain(frames).chain(labels).max().unwrap_or(0) - self.min_v + 1);
        let span_u = cell(self.total_u);
        let legend = self.legend();
        let legend_width = legend
            .iter()
            .flatten()
            .map(|s| canvas::width(&s.text))
            .max()
            .unwrap_or(0);
        let (w, h) = if self.vertical {
            (span_v, span_u)
        } else {
            (span_u, span_v)
        };
        (w.max(legend_width), h + legend.len())
    }

    pub(super) fn draw(&self) -> Vec<Line> {
        let mut canvas = Canvas::default();
        for (g, frame) in self.frames.iter().enumerate() {
            let Some(frame) = frame else { continue };
            let (a, b) = self.corners(
                (self.top_row(frame), frame.lo),
                (self.bottom_row(frame), frame.hi),
            );
            canvas.rect(a, b, BORDER);
            let title = &self.graph.groups[g].title;
            if !title.is_empty() {
                canvas.text((a.0 + 2, a.1), &format!(" {title} "), TITLE);
            }
        }
        for item in &self.items {
            if item.node.is_some() {
                let (a, b) = self.corners(
                    (item.u, item.v),
                    (item.u + item.len_u - 1, item.v + item.len_v - 1),
                );
                canvas.rect(a, b, BORDER);
            }
        }
        for e in 0..self.graph.edges.len() {
            self.edge(&mut canvas, e);
        }
        for item in &self.items {
            if let Some(node) = item.node {
                self.node(&mut canvas, item, node);
            }
        }
        self.edge_labels(&mut canvas);
        let mut lines = canvas.lines();
        lines.extend(self.legend());
        lines
    }

    /// The least `v` anything is drawn at.
    fn lowest_v(&self) -> i64 {
        let items = self.items.iter().map(|item| item.v);
        let frames = self.frames.iter().flatten().map(|f| f.lo);
        let labels = self.segs.iter().filter_map(|s| s.label).map(|(_, v)| v);
        items.chain(frames).chain(labels).min().unwrap_or(0)
    }

    fn edge(&self, canvas: &mut Canvas, e: usize) {
        let edge = &self.graph.edges[e];
        let (Some(stroke), Some(&first), Some(&last)) =
            (edge.stroke, self.chains[e].first(), self.chains[e].last())
        else {
            return;
        };
        let (a, b) = (
            &self.items[self.segs[first].upper],
            &self.items[self.segs[last].lower],
        );
        let mut points = vec![(a.u + a.len_u, self.segs[first].src)];
        for &s in &self.chains[e] {
            let seg = &self.segs[s];
            if let Some(track) = seg.track {
                let u = self.tracks_at(self.items[seg.upper].layer) + track;
                points.extend([(u, seg.src), (u, seg.dst)]);
            }
        }
        points.push((b.u - 1, self.segs[last].dst));
        let cells: Vec<(usize, usize)> = points.iter().map(|&(u, v)| self.at(u, v)).collect();
        for pair in cells.windows(2) {
            canvas.line(pair[0], pair[1], stroke, EDGE);
        }
        let (upper, lower) = if self.reversed[e] {
            (edge.heads[1], edge.heads[0])
        } else {
            (edge.heads[0], edge.heads[1])
        };
        let start = self.at(a.u + a.len_u - 1, self.segs[first].src);
        let end = self.at(b.u, self.segs[last].dst);
        tip(canvas, start, cells[0], upper, stroke);
        tip(canvas, end, cells[cells.len() - 1], lower, stroke);
    }

    /// A node's shape and its label, centered.
    fn node(&self, canvas: &mut Canvas, item: &Item, node: usize) {
        let (a, b) = self.corners(
            (item.u, item.v),
            (item.u + item.len_u - 1, item.v + item.len_v - 1),
        );
        let (corners, sides) = outline(self.graph.nodes[node].shape);
        let spots = [(a.0, a.1), (b.0, a.1), (a.0, b.1), (b.0, b.1)];
        let own = [DOWN | RIGHT, DOWN | LEFT, UP | RIGHT, UP | LEFT];
        for ((spot, dirs), c) in spots.into_iter().zip(own).zip(corners) {
            if canvas.dirs(spot.0, spot.1) == dirs {
                canvas.put(spot, c, BORDER);
            }
        }
        if let Some((left, right)) = sides {
            for y in a.1 + 1..b.1 {
                for (x, c) in [(a.0, left), (b.0, right)] {
                    if canvas.dirs(x, y) == UP | DOWN {
                        canvas.put((x, y), c, BORDER);
                    }
                }
            }
        }
        let lines = &self.labels[node];
        let rows = b.1 - a.1 - 1;
        let top = a.1 + 1 + rows.saturating_sub(lines.len()) / 2;
        for (k, line) in lines.iter().enumerate() {
            let room = b.0 - a.0 - 1;
            let x = a.0 + 1 + room.saturating_sub(canvas::width(line)) / 2;
            canvas.text((x, top + k), line, PLAIN);
        }
    }

    fn edge_labels(&self, canvas: &mut Canvas) {
        for (s, seg) in self.segs.iter().enumerate() {
            let Some(text) = self.label(s) else { continue };
            let text = format!(" {text} ");
            let layer = self.items[seg.upper].layer;
            let at = if let Some((row, v)) = seg.label {
                self.at(self.labels_at(layer) + row, v)
            } else if !self.vertical {
                let len = int(canvas::width(&text));
                let u = self.labels_at(layer) + (self.label_len[layer] - len) / 2;
                let (first, last) = (self.at(u, seg.dst), self.at(u + len - 1, seg.dst));
                (first.0.min(last.0), first.1)
            } else {
                continue;
            };
            canvas.text(at, &text, EDGE_LABEL);
        }
    }

    /// Lines naming the members of subgraphs whose frames couldn't be
    /// drawn.
    fn legend(&self) -> Vec<Line> {
        let mut lines = Vec::new();
        for (g, group) in self.graph.groups.iter().enumerate() {
            if self.frames[g].is_some() {
                continue;
            }
            let members: Vec<&str> = self
                .graph
                .nodes
                .iter()
                .filter(|node| node.group == Some(g))
                .map(|node| node.label.lines().next().unwrap_or_default())
                .collect();
            if members.is_empty() {
                continue;
            }
            lines.push(vec![Span {
                text: format!("subgraph {}: {}", group.title, members.join(", ")),
                style: BORDER,
            }]);
        }
        lines
    }
}

/// An edge's end where it meets a node's border at `border`, from its
/// last cell `cell`: a head in that cell, or the line joined to the border.
fn tip(
    canvas: &mut Canvas,
    border: (usize, usize),
    cell: (usize, usize),
    head: Head,
    stroke: Stroke,
) {
    let toward = if border.1 > cell.1 {
        DOWN
    } else if border.1 < cell.1 {
        UP
    } else if border.0 > cell.0 {
        RIGHT
    } else {
        LEFT
    };
    let glyph = match head {
        Head::None => {
            let back = match toward {
                DOWN => UP,
                UP => DOWN,
                RIGHT => LEFT,
                _ => RIGHT,
            };
            canvas.mark(border, back, stroke, EDGE);
            canvas.mark(cell, toward, stroke, EDGE);
            return;
        }
        Head::Circle => '○',
        Head::Cross => '×',
        Head::Arrow => match toward {
            DOWN => '▼',
            UP => '▲',
            RIGHT => '▶',
            _ => '◀',
        },
    };
    canvas.put(cell, glyph, EDGE);
}

/// A shape's corners (top-left, top-right, bottom-left, bottom-right) and
/// its sides, where they differ from a plain box's.
fn outline(shape: Shape) -> ([char; 4], Option<(char, char)>) {
    const BOX: [char; 4] = ['┌', '┐', '└', '┘'];
    const ROUND: [char; 4] = ['╭', '╮', '╰', '╯'];
    const SLANT: [char; 4] = ['╱', '╲', '╲', '╱'];
    match shape {
        Shape::Rect => (BOX, None),
        Shape::Round | Shape::Cylinder => (ROUND, None),
        Shape::Stadium | Shape::Circle => (ROUND, Some(('(', ')'))),
        Shape::Subroutine => (['╓', '╖', '╙', '╜'], Some(('║', '║'))),
        Shape::Diamond => (SLANT, Some(('<', '>'))),
        Shape::Hexagon => (SLANT, None),
        Shape::Flag => (['╲', '┐', '╱', '┘'], Some(('>', '│'))),
        Shape::Lean => (BOX, Some(('╱', '╱'))),
    }
}

/// The rows `count` nested frame borders take at one boundary, a blank
/// row between each two.
fn borders(count: i64) -> i64 {
    (2 * count - 1).max(0)
}

/// Where the `k`th of `m` ports goes on a side from `v` that's `len` long,
/// corners excluded.
fn port(v: i64, len: i64, k: usize, m: usize) -> i64 {
    if len <= 2 {
        return v;
    }
    let inner = len - 2;
    v + 1 + (2 * int(k) + 1) * inner / (2 * int(m))
}

/// `row` sorted by `key`, the items of each subgraph kept together and
/// the subgraphs placed by their items' mean key, at every nesting level
/// from `level` in. `chains` holds each item's subgraphs, outermost first.
fn arrange(row: &[usize], key: &[i64], level: usize, chains: &[Vec<usize>]) -> Vec<usize> {
    struct Unit {
        group: Option<usize>,
        sum: i64,
        items: Vec<usize>,
    }
    let mut units: Vec<Unit> = Vec::new();
    let mut found: HashMap<usize, usize> = HashMap::new();
    for &i in row {
        let group = chains[i].get(level).copied();
        if let Some(&u) = group.and_then(|g| found.get(&g)) {
            units[u].sum += key[i];
            units[u].items.push(i);
        } else {
            if let Some(g) = group {
                found.insert(g, units.len());
            }
            units.push(Unit {
                group,
                sum: key[i],
                items: vec![i],
            });
        }
    }
    units.sort_by(|a, b| (a.sum * int(b.items.len())).cmp(&(b.sum * int(a.items.len()))));
    units
        .into_iter()
        .flat_map(|unit| match unit.group {
            Some(_) => arrange(&unit.items, key, level + 1, chains),
            None => unit.items,
        })
        .collect()
}

/// The pairs out of order in `v`, which it sorts.
fn inversions(v: &mut [i64]) -> usize {
    if v.len() < 2 {
        return 0;
    }
    let mid = v.len() / 2;
    let mut count = inversions(&mut v[..mid]) + inversions(&mut v[mid..]);
    let mut merged = Vec::with_capacity(v.len());
    let (mut i, mut j) = (0, mid);
    while i < mid && j < v.len() {
        if v[i] <= v[j] {
            merged.push(v[i]);
            i += 1;
        } else {
            merged.push(v[j]);
            count += mid - i;
            j += 1;
        }
    }
    merged.extend_from_slice(&v[i..mid]);
    merged.extend_from_slice(&v[j..]);
    v.copy_from_slice(&merged);
    count
}

/// Which edges a depth-first search finds going back to a node it's still
/// in: turned around, they leave the graph without cycles.
fn back_edges(graph: &Graph) -> Vec<bool> {
    let n = graph.nodes.len();
    let mut out = vec![Vec::new(); n];
    for (e, edge) in graph.edges.iter().enumerate() {
        if edge.from != edge.to {
            out[edge.from].push((e, edge.to));
        }
    }
    let mut state = vec![0u8; n];
    let mut reversed = vec![false; graph.edges.len()];
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for root in 0..n {
        if state[root] != 0 {
            continue;
        }
        state[root] = 1;
        stack.push((root, 0));
        while let Some(top) = stack.last_mut() {
            let (v, k) = *top;
            if let Some(&(e, w)) = out[v].get(k) {
                top.1 += 1;
                match state[w] {
                    0 => {
                        state[w] = 1;
                        stack.push((w, 0));
                    }
                    1 => reversed[e] = true,
                    _ => {}
                }
            } else {
                state[v] = 2;
                stack.pop();
            }
        }
    }
    reversed
}

/// Each node's layer: its longest path from a source, with sources then
/// moved down to just above their nearest successor.
fn layers(graph: &Graph, reversed: &[bool]) -> Vec<usize> {
    let n = graph.nodes.len();
    let mut out = vec![Vec::new(); n];
    let mut indegree = vec![0; n];
    for (e, edge) in graph.edges.iter().enumerate() {
        if edge.from == edge.to {
            continue;
        }
        let (a, b) = if reversed[e] {
            (edge.to, edge.from)
        } else {
            (edge.from, edge.to)
        };
        out[a].push(b);
        indegree[b] += 1;
    }
    let mut layer = vec![0; n];
    let mut remaining = indegree.clone();
    let mut ready: Vec<usize> = (0..n).rev().filter(|&i| indegree[i] == 0).collect();
    let mut sorted = Vec::with_capacity(n);
    while let Some(a) = ready.pop() {
        sorted.push(a);
        for &b in &out[a] {
            layer[b] = layer[b].max(layer[a] + 1);
            remaining[b] -= 1;
            if remaining[b] == 0 {
                ready.push(b);
            }
        }
    }
    for &a in &sorted {
        if indegree[a] == 0
            && let Some(nearest) = out[a].iter().map(|&b| layer[b]).min()
        {
            layer[a] = nearest - 1;
        }
    }
    layer
}

//! One side's syntax tree flattened for matching: nodes in an arena with
//! structural hashes, and the tokens (leaves) in document order.
//!
//! A hash covers a node's kind and its children's hashes, or a leaf's kind
//! and text, never the whitespace between tokens: two nodes with equal
//! hashes hold the same code however it's laid out. Text no token covers
//! (some grammars hide tokens) becomes a leaf of its own, so every
//! non-whitespace byte takes part in some hash.
//!
//! A node made of punctuation alone (`;`, `{}`, `)`) is not informative:
//! it recurs everywhere, so an equal one on the other side says nothing
//! about where its code went.

use std::hash::{Hash, Hasher};
use std::ops::Range;

use rustc_hash::FxHasher;

use super::index;

/// The kind of a leaf made of text no token covers.
const UNCOVERED: u16 = u16::MAX;

pub(super) const NO_PARENT: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
pub(super) struct Node {
    pub hash: u64,
    pub kind: u16,
    /// A parse-error subtree. Its structure is whatever the parser made of
    /// broken code, so it's matched whole or token by token, never by
    /// shape.
    pub atom: bool,
    /// It is or holds an atom.
    pub has_error: bool,
    /// Holds an identifier, keyword, literal or comment.
    pub informative: bool,
    /// [`NO_PARENT`] for the root.
    pub parent: u32,
    children: (u32, u32),
    leaves: (u32, u32),
    /// Non-whitespace bytes of its leaves.
    pub solid: u32,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Leaf {
    pub start: u32,
    pub end: u32,
    pub hash: u64,
    /// The text has whitespace (comments, strings, prose), so a change in it
    /// is shown word by word.
    pub spaced: bool,
    /// A comment: its hash ignores how the words are spaced.
    pub comment: bool,
    /// A comment, or text with a letter, digit, `_` or non-ASCII byte.
    pub informative: bool,
    /// The node it is, or the atom it's hidden in.
    pub node: u32,
}

#[derive(Debug, Default)]
pub(super) struct Tree {
    pub nodes: Vec<Node>,
    children: Vec<u32>,
    pub leaves: Vec<Leaf>,
    /// The nodes at the top of the file.
    pub top: Vec<u32>,
    /// By kind id, whether nodes of that kind declare a field (struct and
    /// class members), going by the grammar's kind name.
    fields: Vec<bool>,
}

impl Tree {
    pub(super) fn children(&self, node: u32) -> &[u32] {
        let (start, len) = self.nodes[node as usize].children;
        &self.children[start as usize..(start + len) as usize]
    }

    pub(super) fn leaves(&self, node: u32) -> Range<usize> {
        let (start, end) = self.nodes[node as usize].leaves;
        start as usize..end as usize
    }

    pub(super) fn node(&self, node: u32) -> &Node {
        &self.nodes[node as usize]
    }

    /// Where `node`'s last token ends.
    pub(super) fn node_end(&self, node: u32) -> usize {
        self.leaves[self.leaves(node).end - 1].end as usize
    }

    /// Whether `node` is a token (or an atom): matched by text, not by
    /// children.
    pub(super) fn is_leaf(&self, node: u32) -> bool {
        self.nodes[node as usize].children.1 == 0
    }

    pub(super) fn is_field(&self, node: u32) -> bool {
        let kind = usize::from(self.nodes[node as usize].kind);
        self.fields.get(kind).copied().unwrap_or(false)
    }

    pub(super) fn build(tree: &tree_sitter::Tree, source: &[u8]) -> Self {
        let mut b = Builder {
            tree: Self::default(),
            source,
            frames: Vec::new(),
            open: Vec::new(),
            comments: Vec::new(),
            fields: Vec::new(),
        };
        let mut cursor = tree.walk();
        let mut descend = b.visit(cursor.node());
        'walk: loop {
            if descend {
                if cursor.goto_first_child() {
                    descend = b.visit(cursor.node());
                    continue;
                }
                b.finish();
            }
            loop {
                if cursor.goto_next_sibling() {
                    descend = b.visit(cursor.node());
                    continue 'walk;
                }
                if !cursor.goto_parent() {
                    break 'walk;
                }
                b.finish();
            }
        }
        let mut tree = b.tree;
        tree.fields = b.fields.into_iter().map(|f| f == Some(true)).collect();
        tree.top = match tree.nodes.len().checked_sub(1) {
            Some(root) => {
                let root = index(root);
                if tree.is_leaf(root) {
                    vec![root]
                } else {
                    tree.children(root).to_vec()
                }
            }
            None => Vec::new(),
        };
        tree
    }
}

/// A node being built: its children are still being visited.
struct Frame {
    kind: u16,
    end: usize,
    /// Where the previous child ended, for uncovered text between children.
    last_end: usize,
    /// Where its children's ids start in `Builder::open`.
    first_child: usize,
    /// Where its leaves start in the tree's leaves.
    first_leaf: usize,
    atom: bool,
    /// Inside an atom: no node of its own, only its leaves count.
    hidden: bool,
}

struct Builder<'a> {
    tree: Tree,
    source: &'a [u8],
    frames: Vec<Frame>,
    /// Ids of the finished children of the open frames.
    open: Vec<u32>,
    /// By kind id, whether leaves of that kind are comments, once known.
    comments: Vec<Option<bool>>,
    /// By kind id, whether nodes of that kind are fields, once known.
    fields: Vec<Option<bool>>,
}

impl Builder<'_> {
    /// Adds `node`, or opens a frame for it when it has children. Returns
    /// whether a frame was opened.
    fn visit(&mut self, node: tree_sitter::Node) -> bool {
        let (start, end) = (node.start_byte(), node.end_byte());
        // Tokens the parser inserted to recover have no text.
        if node.is_missing() || start == end {
            return false;
        }
        let hidden = self.frames.last().is_some_and(|f| f.atom || f.hidden);
        if let Some(from) = self.frames.last().map(|f| f.last_end) {
            self.uncovered(from, start, hidden);
        }
        if let Some(f) = self.frames.last_mut() {
            f.last_end = end;
        }
        if node.child_count() == 0 {
            let kind = node.kind_id();
            let at = usize::from(kind);
            if self.comments.len() <= at {
                self.comments.resize(at + 1, None);
            }
            let comment = *self.comments[at].get_or_insert_with(|| node.kind().contains("comment"));
            let leaf = self.leaf(start, end, kind, comment);
            if !hidden {
                let solid = solid(&self.source[start..end]);
                self.push_node(kind, false, self.open.len(), leaf..leaf + 1, solid);
            }
            return false;
        }
        let at = usize::from(node.kind_id());
        if self.fields.len() <= at {
            self.fields.resize(at + 1, None);
        }
        self.fields[at].get_or_insert_with(|| {
            let kind = node.kind();
            kind.contains("field_declaration") || kind.contains("field_definition")
        });
        self.frames.push(Frame {
            kind: node.kind_id(),
            end,
            last_end: start,
            first_child: self.open.len(),
            first_leaf: self.tree.leaves.len(),
            atom: node.is_error() && !hidden,
            hidden,
        });
        true
    }

    /// Closes the innermost frame.
    fn finish(&mut self) {
        let Some(f) = self.frames.pop() else { return };
        self.uncovered(f.last_end, f.end, f.hidden || f.atom);
        let leaves = f.first_leaf..self.tree.leaves.len();
        if f.hidden || leaves.is_empty() {
            self.open.truncate(f.first_child);
            return;
        }
        if f.atom {
            let solid = leaves
                .clone()
                .map(|i| {
                    let l = self.tree.leaves[i];
                    solid(&self.source[l.start as usize..l.end as usize])
                })
                .sum();
            self.push_node(f.kind, true, self.open.len(), leaves, solid);
            return;
        }
        let solid = self.open[f.first_child..]
            .iter()
            .map(|&c| self.tree.nodes[c as usize].solid)
            .sum();
        self.push_node(f.kind, false, f.first_child, leaves, solid);
    }

    /// Adds the text between `from` and `to` as a leaf when it isn't all
    /// whitespace.
    fn uncovered(&mut self, from: usize, to: usize, hidden: bool) {
        if from >= to {
            return;
        }
        let gap = &self.source[from..to];
        let (Some(first), Some(last)) = (
            gap.iter().position(|b| !b.is_ascii_whitespace()),
            gap.iter().rposition(|b| !b.is_ascii_whitespace()),
        ) else {
            return;
        };
        let (start, end) = (from + first, from + last + 1);
        let leaf = self.leaf(start, end, UNCOVERED, false);
        if !hidden {
            let solid = solid(&self.source[start..end]);
            self.push_node(UNCOVERED, false, self.open.len(), leaf..leaf + 1, solid);
        }
    }

    fn leaf(&mut self, start: usize, end: usize, kind: u16, comment: bool) -> usize {
        let text = &self.source[start..end];
        let mut h = FxHasher::default();
        kind.hash(&mut h);
        if comment {
            // Reflowing a comment doesn't change it.
            for word in text
                .split(u8::is_ascii_whitespace)
                .filter(|w| !w.is_empty())
            {
                h.write(word);
                h.write_u8(0);
            }
        } else {
            h.write(text);
        }
        self.tree.leaves.push(Leaf {
            start: index(start),
            end: index(end),
            hash: h.finish(),
            spaced: text.iter().any(u8::is_ascii_whitespace),
            comment,
            informative: comment
                || text
                    .iter()
                    .any(|&b| b.is_ascii_alphanumeric() || b == b'_' || !b.is_ascii()),
            node: NO_PARENT,
        });
        self.tree.leaves.len() - 1
    }

    /// Adds a node whose children are `self.open[children..]`, taking them
    /// off the list of open children.
    fn push_node(
        &mut self,
        kind: u16,
        atom: bool,
        children: usize,
        leaves: Range<usize>,
        solid: u32,
    ) {
        let children_ids = &self.open[children..];
        let mut h = FxHasher::default();
        kind.hash(&mut h);
        if children_ids.is_empty() {
            for leaf in &self.tree.leaves[leaves.clone()] {
                h.write_u64(leaf.hash);
            }
        } else {
            for &c in children_ids {
                h.write_u64(self.tree.nodes[c as usize].hash);
            }
        }
        let has_error = atom
            || children_ids
                .iter()
                .any(|&c| self.tree.nodes[c as usize].has_error);
        let id = index(self.tree.nodes.len());
        let informative = if children_ids.is_empty() {
            let leaves = &mut self.tree.leaves[leaves.clone()];
            for leaf in leaves.iter_mut() {
                leaf.node = id;
            }
            leaves.iter().any(|l| l.informative)
        } else {
            children_ids
                .iter()
                .any(|&c| self.tree.nodes[c as usize].informative)
        };
        for &c in children_ids {
            self.tree.nodes[c as usize].parent = id;
        }
        let start = index(self.tree.children.len());
        let count = index(children_ids.len());
        self.tree.children.extend_from_slice(children_ids);
        self.open.truncate(children);
        self.tree.nodes.push(Node {
            hash: h.finish(),
            kind,
            atom,
            has_error,
            informative,
            parent: NO_PARENT,
            children: (start, count),
            leaves: (index(leaves.start), index(leaves.end)),
            solid,
        });
        self.open.push(id);
    }
}

fn solid(text: &[u8]) -> u32 {
    index(text.iter().filter(|b| !b.is_ascii_whitespace()).count())
}

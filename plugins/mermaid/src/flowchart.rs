//! Flowcharts (`graph` / `flowchart`): nodes with their shapes, edges with
//! their strokes, heads and labels, and subgraphs, parsed from Mermaid's
//! syntax and handed to the layered layout in [`layout`].
//!
//! Styling statements (`classDef`, `class`, `style`, `linkStyle`, `click`)
//! are skipped. An edge to a subgraph's id is drawn to a node of that name.

mod layout;

use std::collections::HashMap;

use crate::canvas::Stroke;
use crate::{Line, MAX_CELLS, clean};

/// Past these sizes the source shows instead: a drawing that large isn't
/// readable as text, and the layout's cost grows with edges times layers
/// and with subgraphs squared. The parser stops as soon as one is passed.
const MAX_NODES: usize = 300;
const MAX_EDGES: usize = 600;
const MAX_GROUPS: usize = 100;

/// Node label wrap widths and gaps between nodes to try, widest first,
/// until the drawing fits the width it's given.
const ATTEMPTS: [(usize, i64); 4] = [(usize::MAX, 2), (24, 2), (16, 1), (10, 1)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Down,
    Up,
    Right,
    Left,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Rect,
    Round,
    Stadium,
    Circle,
    Subroutine,
    Cylinder,
    Diamond,
    Hexagon,
    Flag,
    Lean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Head {
    None,
    Arrow,
    Circle,
    Cross,
}

#[derive(Debug)]
struct Node {
    label: String,
    shape: Shape,
    /// The innermost subgraph it's in.
    group: Option<usize>,
}

#[derive(Debug)]
struct Edge {
    from: usize,
    to: usize,
    /// `None` for an invisible link (`~~~`), which only spaces nodes.
    stroke: Option<Stroke>,
    /// At `from` and at `to`.
    heads: [Head; 2],
    label: Option<String>,
}

#[derive(Debug)]
struct Group {
    title: String,
    parent: Option<usize>,
}

#[derive(Debug)]
struct Graph {
    direction: Direction,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    groups: Vec<Group>,
}

impl Graph {
    /// The subgraphs `group` is in, outermost first, ending with it.
    fn ancestry(&self, group: Option<usize>) -> Vec<usize> {
        let mut chain = Vec::new();
        let mut at = group;
        while let Some(g) = at {
            chain.push(g);
            at = self.groups[g].parent;
        }
        chain.reverse();
        chain
    }

    fn depth(&self, group: Option<usize>) -> usize {
        self.ancestry(group).len()
    }

    /// The innermost subgraph holding both `a` and `b`.
    fn common(&self, a: Option<usize>, b: Option<usize>) -> Option<usize> {
        let (a, b) = (self.ancestry(a), self.ancestry(b));
        a.iter()
            .zip(&b)
            .take_while(|(x, y)| x == y)
            .last()
            .map(|(x, _)| *x)
    }

    /// Whether `inner` is `group` or inside it.
    fn contains(&self, group: usize, inner: Option<usize>) -> bool {
        let mut at = inner;
        while let Some(g) = at {
            if g == group {
                return true;
            }
            at = self.groups[g].parent;
        }
        false
    }
}

/// The flowchart `statements` describe (the first is its `graph` line),
/// drawn to fit `width` when one of the attempts does, else the narrowest.
pub(crate) fn render(statements: &[(usize, &str)], width: usize) -> Option<Vec<Line>> {
    let graph = parse(statements)?;
    if graph.nodes.is_empty() {
        return None;
    }
    let mut chosen: Option<(layout::Layout<'_>, usize, usize)> = None;
    for (wrap_at, gap) in ATTEMPTS {
        let layout = layout::Layout::new(&graph, wrap_at, gap)?;
        let (w, h) = layout.size();
        if chosen
            .as_ref()
            .is_none_or(|(_, narrowest, _)| w < *narrowest)
        {
            chosen = Some((layout, w, h));
        }
        if w <= width {
            break;
        }
    }
    let (layout, w, h) = chosen?;
    (w.saturating_mul(h) <= MAX_CELLS).then(|| layout.draw())
}

fn parse(statements: &[(usize, &str)]) -> Option<Graph> {
    let (&(_, header), rest) = statements.split_first()?;
    let mut pieces = split(header).into_iter();
    let mut words = pieces.next()?.split_whitespace().skip(1);
    let direction = match words.next() {
        None | Some("TD" | "TB") => Direction::Down,
        Some("BT") => Direction::Up,
        Some("LR") => Direction::Right,
        Some("RL") => Direction::Left,
        Some(_) => return None,
    };
    if words.next().is_some() {
        return None;
    }
    let mut parser = Parser {
        graph: Graph {
            direction,
            nodes: Vec::new(),
            edges: Vec::new(),
            groups: Vec::new(),
        },
        ids: HashMap::new(),
        open: Vec::new(),
    };
    for statement in pieces.chain(rest.iter().flat_map(|(_, s)| split(s))) {
        parser.statement(statement)?;
    }
    parser.open.is_empty().then_some(parser.graph)
}

/// `line` split at the semicolons outside quotes and brackets.
fn split(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut quoted, mut start) = (0usize, false, 0);
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '[' | '(' | '{' if !quoted => depth += 1,
            ']' | ')' | '}' if !quoted => depth = depth.saturating_sub(1),
            ';' if !quoted && depth == 0 => {
                out.push(&line[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&line[start..]);
    out
}

struct Parser {
    graph: Graph,
    ids: HashMap<String, usize>,
    /// The subgraphs open where the parser is, innermost last.
    open: Vec<usize>,
}

impl Parser {
    fn statement(&mut self, statement: &str) -> Option<()> {
        let statement = statement.trim();
        let (word, rest) = statement
            .split_once(char::is_whitespace)
            .unwrap_or((statement, ""));
        match word {
            "" | "direction" | "classDef" | "class" | "style" | "linkStyle" | "click" => {}
            _ if word.starts_with("accTitle") || word.starts_with("accDescr") => {}
            "subgraph" => self.subgraph(rest)?,
            "end" => {
                self.open.pop()?;
            }
            _ => self.chain(statement)?,
        }
        Some(())
    }

    /// `subgraph id [title]`, `subgraph "title"` or `subgraph title`.
    fn subgraph(&mut self, rest: &str) -> Option<()> {
        if self.graph.groups.len() >= MAX_GROUPS {
            return None;
        }
        let rest = rest.trim();
        let title = match rest.split_once('[') {
            Some((_, title)) => clean(title.trim_end().strip_suffix(']').unwrap_or(title)),
            None => clean(rest),
        };
        self.graph.groups.push(Group {
            title: title.replace('\n', " "),
            parent: self.open.last().copied(),
        });
        self.open.push(self.graph.groups.len() - 1);
        Some(())
    }

    /// A node mentioned: created on first mention, given the shape and
    /// label of a mention that has them, and moved into the subgraph of a
    /// mention inside one. `None` past [`MAX_NODES`].
    fn node(&mut self, id: &str, shape: Option<(Shape, String)>) -> Option<usize> {
        let index = if let Some(&index) = self.ids.get(id) {
            index
        } else {
            if self.graph.nodes.len() >= MAX_NODES {
                return None;
            }
            self.graph.nodes.push(Node {
                label: id.to_owned(),
                shape: Shape::Rect,
                group: None,
            });
            self.ids.insert(id.to_owned(), self.graph.nodes.len() - 1);
            self.graph.nodes.len() - 1
        };
        let node = &mut self.graph.nodes[index];
        if let Some(&group) = self.open.last() {
            node.group = Some(group);
        }
        if let Some((shape, label)) = shape {
            node.shape = shape;
            node.label = label;
        }
        Some(index)
    }

    /// `A --> B & C -- label --> D`: every node of a group linked to every
    /// node of the next.
    fn chain(&mut self, statement: &str) -> Option<()> {
        let mut cursor = Cursor {
            chars: statement.chars().collect(),
            i: 0,
        };
        let mut previous = self.group(&mut cursor)?;
        loop {
            cursor.skip_spaces();
            if cursor.done() {
                return Some(());
            }
            let (stroke, heads, label) = cursor.edge()?;
            cursor.skip_spaces();
            let next = self.group(&mut cursor)?;
            if self.graph.edges.len() + previous.len() * next.len() > MAX_EDGES {
                return None;
            }
            for &from in &previous {
                for &to in &next {
                    self.graph.edges.push(Edge {
                        from,
                        to,
                        stroke,
                        heads,
                        label: label.clone(),
                    });
                }
            }
            previous = next;
        }
    }

    fn group(&mut self, cursor: &mut Cursor) -> Option<Vec<usize>> {
        let mut nodes = vec![self.one(cursor)?];
        loop {
            let at = cursor.i;
            cursor.skip_spaces();
            if cursor.peek() == Some('&') && nodes.len() < MAX_EDGES {
                cursor.i += 1;
                cursor.skip_spaces();
                nodes.push(self.one(cursor)?);
            } else {
                cursor.i = at;
                return Some(nodes);
            }
        }
    }

    fn one(&mut self, cursor: &mut Cursor) -> Option<usize> {
        let id = cursor.take(|c| c.is_alphanumeric() || c == '_');
        if id.is_empty() {
            return None;
        }
        let shape = cursor.shape(&id).ok()?;
        if cursor.starts(":::") {
            cursor.i += 3;
            cursor.take(|c| c.is_alphanumeric() || c == '_' || c == '-');
        }
        self.node(&id, shape)
    }
}

/// Node shapes by their opening bracket, longest first, with the brackets
/// that close them.
const SHAPES: [(&str, &[&str], Shape); 12] = [
    ("(((", &[")))"], Shape::Circle),
    ("((", &["))"], Shape::Circle),
    ("([", &["])"], Shape::Stadium),
    ("[[", &["]]"], Shape::Subroutine),
    ("[(", &[")]"], Shape::Cylinder),
    ("[/", &["/]", "\\]"], Shape::Lean),
    ("[\\", &["\\]", "/]"], Shape::Lean),
    ("{{", &["}}"], Shape::Hexagon),
    ("{", &["}"], Shape::Diamond),
    ("(", &[")"], Shape::Round),
    ("[", &["]"], Shape::Rect),
    (">", &["]"], Shape::Flag),
];

struct Cursor {
    chars: Vec<char>,
    i: usize,
}

/// A node's brackets that don't close.
struct Unclosed;

impl Cursor {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn done(&self) -> bool {
        self.i >= self.chars.len()
    }

    fn skip_spaces(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.i += 1;
        }
    }

    fn starts_at(&self, at: usize, text: &str) -> bool {
        text.chars()
            .enumerate()
            .all(|(k, c)| self.chars.get(at + k) == Some(&c))
    }

    fn starts(&self, text: &str) -> bool {
        self.starts_at(self.i, text)
    }

    fn find(&self, text: &str) -> Option<usize> {
        (self.i..self.chars.len()).find(|&at| self.starts_at(at, text))
    }

    fn take(&mut self, keep: impl Fn(char) -> bool) -> String {
        let start = self.i;
        while self.peek().is_some_and(&keep) {
            self.i += 1;
        }
        self.chars[start..self.i].iter().collect()
    }

    fn text(&self, start: usize, end: usize) -> String {
        self.chars[start..end].iter().collect()
    }

    /// The shape and label in brackets after a node's id, if any.
    fn shape(&mut self, id: &str) -> Result<Option<(Shape, String)>, Unclosed> {
        if self.starts("@{") {
            let end = self.find("}").ok_or(Unclosed)?;
            let body = self.text(self.i + 2, end);
            self.i = end + 1;
            return Ok(Some(described(&body, id)));
        }
        let Some(&(open, closers, shape)) = SHAPES.iter().find(|(open, ..)| self.starts(open))
        else {
            return Ok(None);
        };
        self.i += open.chars().count();
        self.skip_spaces();
        let label = if self.peek() == Some('"') {
            let end = (self.i + 1..self.chars.len())
                .find(|&j| self.chars[j] == '"')
                .ok_or(Unclosed)?;
            let label = self.text(self.i + 1, end);
            self.i = end + 1;
            self.skip_spaces();
            let close = closers.iter().find(|c| self.starts(c)).ok_or(Unclosed)?;
            self.i += close.chars().count();
            label
        } else {
            let (end, close) = (self.i..self.chars.len())
                .find_map(|at| {
                    closers
                        .iter()
                        .find(|c| self.starts_at(at, c))
                        .map(|c| (at, c))
                })
                .ok_or(Unclosed)?;
            let label = self.text(self.i, end);
            self.i = end + close.chars().count();
            label
        };
        Ok(Some((shape, clean(&label))))
    }

    /// A link: its stroke, heads and label, as in `-->`, `-.->`, `==>`,
    /// `--o`, `<-->`, `-- text -->` or `-->|text|`.
    fn edge(&mut self) -> Option<(Option<Stroke>, [Head; 2], Option<String>)> {
        let mut heads = [Head::None; 2];
        if self.starts("~~~") {
            self.take(|c| c == '~');
            return Some((None, heads, self.pipe_label()));
        }
        let next = self.chars.get(self.i + 1).copied();
        match self.peek() {
            Some('<') => {
                heads[0] = Head::Arrow;
                self.i += 1;
            }
            Some(c @ ('o' | 'x')) if matches!(next, Some('-' | '=' | '.')) => {
                heads[0] = head(c);
                self.i += 1;
            }
            _ => {}
        }
        let body = self.take(|c| matches!(c, '-' | '=' | '.'));
        if body.chars().count() < 2 {
            return None;
        }
        let mut end = self.head();
        let mut label = None;
        // `-- text -->`: the opening half sets the stroke.
        if end == Head::None && matches!(body.as_str(), "--" | "==" | "-.") {
            let close = if body == "-." { ".-" } else { body.as_str() };
            let at = self.find(close)?;
            label = Some(clean(&self.text(self.i, at)));
            self.i = at;
            self.take(|c| matches!(c, '-' | '=' | '.'));
            end = self.head();
        }
        heads[1] = end;
        if let Some(text) = self.pipe_label() {
            label = Some(text);
        }
        let label = label
            .map(|l| l.replace('\n', " "))
            .filter(|l| !l.is_empty());
        Some((Some(stroke(&body)), heads, label))
    }

    fn head(&mut self) -> Head {
        let found = match self.peek() {
            Some('>') => Head::Arrow,
            Some(c @ ('o' | 'x')) => head(c),
            _ => return Head::None,
        };
        self.i += 1;
        found
    }

    /// `|text|` after a link.
    fn pipe_label(&mut self) -> Option<String> {
        let at = self.i;
        self.skip_spaces();
        if self.peek() == Some('|') {
            self.i += 1;
            if let Some(end) = self.find("|") {
                let label = clean(&self.text(self.i, end));
                self.i = end + 1;
                return Some(label);
            }
        }
        self.i = at;
        None
    }
}

fn head(c: char) -> Head {
    if c == 'o' { Head::Circle } else { Head::Cross }
}

fn stroke(body: &str) -> Stroke {
    if body.contains('=') {
        Stroke::Thick
    } else if body.contains('.') {
        Stroke::Dotted
    } else {
        Stroke::Light
    }
}

/// The shape and label of the `@{ shape: ..., label: "..." }` syntax.
fn described(body: &str, id: &str) -> (Shape, String) {
    let field = |name: &str| {
        let at = body.find(&format!("{name}:"))? + name.len() + 1;
        let rest = body[at..].trim_start();
        let value = match rest.strip_prefix('"') {
            Some(quoted) => quoted.split('"').next().unwrap_or_default(),
            None => rest.split(',').next().unwrap_or_default().trim(),
        };
        Some(value.to_owned())
    };
    let shape = match field("shape").as_deref() {
        Some("rounded" | "event") => Shape::Round,
        Some("stadium" | "terminal" | "pill") => Shape::Stadium,
        Some("circle" | "circ" | "dbl-circ" | "double-circle") => Shape::Circle,
        Some("diamond" | "diam" | "decision" | "question") => Shape::Diamond,
        Some("hex" | "hexagon" | "prepare") => Shape::Hexagon,
        Some("subroutine" | "subproc" | "fr-rect") => Shape::Subroutine,
        Some("cyl" | "cylinder" | "database" | "db") => Shape::Cylinder,
        _ => Shape::Rect,
    };
    let label = field("label").map_or_else(|| id.to_owned(), |l| clean(&l));
    (shape, label)
}

#[cfg(test)]
mod tests;

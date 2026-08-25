//! Server-side Mermaid flowcharts, rendered to inline SVG.
//!
//! Mermaid is a JavaScript library; this is a from-scratch renderer for the
//! flowchart subset of its syntax, so diagrams arrive already drawn and the
//! page ships no script. It parses a ```mermaid fence into a graph, lays the
//! graph out with a layered (Sugiyama-style) algorithm, and writes the SVG
//! itself — which also means every label is escaped by code in this file
//! rather than by a third party.
//!
//! Supported: `flowchart`/`graph` in all five directions, eight node shapes,
//! six link styles, and `|edge labels|`. Not supported: subgraphs, other
//! diagram types (sequence, class, gantt, ...), `<br/>` in labels, and the
//! `-- label -->` inline label form. Anything it cannot parse is left as an
//! ordinary code block.

use comrak::nodes::{AstNode, NodeValue};
use comrak::Arena;

use super::Plugin;
use crate::template::escape_html;

/// Diagram colours, as tokens so the same SVG works in light and dark mode.
/// `currentColor` is deliberately avoided: these need to differ from body text.
const STYLE: &str = concat!(
    "<style>",
    ".mmd{display:block;max-width:100%;height:auto;margin:1em 0;",
    "--mmd-node:#f6f8fa;--mmd-line:#57606a;--mmd-fg:#1f2328;--mmd-label:#ffffff}",
    "@media(prefers-color-scheme:dark){.mmd{",
    "--mmd-node:#161b22;--mmd-line:#8b949e;--mmd-fg:#e6edf3;--mmd-label:#0d1117}}",
    ".mmd .n{fill:var(--mmd-node);stroke:var(--mmd-line);stroke-width:1.5}",
    ".mmd .e{stroke:var(--mmd-line);stroke-width:1.5;fill:none}",
    ".mmd .a{fill:var(--mmd-line)}",
    ".mmd .lb{fill:var(--mmd-label);stroke:none}",
    ".mmd text{fill:var(--mmd-fg);font-family:system-ui,-apple-system,Helvetica,Arial,sans-serif;",
    "font-size:14px}",
    ".mmd .dash{stroke-dasharray:5 4}",
    ".mmd .thick{stroke-width:3}",
    "</style>"
);

pub struct Mermaid;

impl Plugin for Mermaid {
    fn name(&self) -> &'static str {
        "mermaid"
    }

    fn describe(&self) -> &'static str {
        "render mermaid flowcharts as inline SVG"
    }

    fn transform<'a>(&self, _arena: &'a Arena<'a>, root: &'a AstNode<'a>) -> bool {
        let mut found = false;
        for node in root.descendants() {
            let replacement = {
                let ast = node.data.borrow();
                match &ast.value {
                    NodeValue::CodeBlock(cb)
                        if cb.info.split_whitespace().next() == Some("mermaid") =>
                    {
                        parse(&cb.literal).map(|g| to_svg(&layout(&g), &g))
                    }
                    _ => None,
                }
            };
            if let Some(svg) = replacement {
                node.data.borrow_mut().value = NodeValue::Raw(svg);
                found = true;
            }
        }
        found
    }

    fn head(&self) -> Option<&'static str> {
        Some(STYLE)
    }
}

// ---------------------------------------------------------------- model

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Down,
    Up,
    Right,
    Left,
}

impl Direction {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "TD" | "TB" => Some(Self::Down),
            "BT" => Some(Self::Up),
            "LR" => Some(Self::Right),
            "RL" => Some(Self::Left),
            _ => None,
        }
    }

    /// Whether ranks advance along the y axis (top-to-bottom or bottom-to-top).
    fn vertical(self) -> bool {
        matches!(self, Self::Down | Self::Up)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Rect,
    Round,
    Stadium,
    Circle,
    Diamond,
    Hexagon,
    Subroutine,
    Cylinder,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Link {
    Arrow,
    Line,
    DottedArrow,
    DottedLine,
    ThickArrow,
    ThickLine,
}

impl Link {
    fn has_arrow(self) -> bool {
        matches!(self, Self::Arrow | Self::DottedArrow | Self::ThickArrow)
    }

    fn class(self) -> &'static str {
        match self {
            Self::DottedArrow | Self::DottedLine => "e dash",
            Self::ThickArrow | Self::ThickLine => "e thick",
            _ => "e",
        }
    }
}

struct Node {
    label: String,
    shape: Shape,
}

struct Edge {
    from: usize,
    to: usize,
    label: Option<String>,
    link: Link,
}

struct Graph {
    dir: Direction,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    ids: Vec<String>,
}

impl Graph {
    /// Returns the index for `id`, creating a default node on first mention —
    /// Mermaid lets an edge introduce a node it has never seen declared.
    fn node(&mut self, id: &str) -> usize {
        if let Some(i) = self.ids.iter().position(|n| n == id) {
            return i;
        }
        self.ids.push(id.to_string());
        self.nodes.push(Node {
            label: id.to_string(),
            shape: Shape::Rect,
        });
        self.nodes.len() - 1
    }
}

// --------------------------------------------------------------- parsing

/// Parses a flowchart, or `None` if this is not one we can draw.
///
/// Unrecognised statements inside a flowchart (`subgraph`, `style`, `class`,
/// `click`, ...) are skipped rather than failing the whole diagram; a header
/// naming a different diagram type fails outright so the fence stays a code
/// block instead of rendering as an empty box.
fn parse(src: &str) -> Option<Graph> {
    let mut dir = Direction::Down;
    let mut header_seen = false;
    let mut graph = Graph {
        dir,
        nodes: Vec::new(),
        edges: Vec::new(),
        ids: Vec::new(),
    };

    for raw_line in src.lines() {
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if !header_seen {
            let (kind, rest) = split_word(line);
            if kind != "flowchart" && kind != "graph" {
                return None; // sequenceDiagram, classDiagram, ...
            }
            header_seen = true;
            if let Some(d) = Direction::parse(rest.trim()) {
                dir = d;
            }
            continue;
        }
        for stmt in line.split(';') {
            let stmt = stmt.trim();
            if stmt.is_empty() || is_directive(stmt) {
                continue;
            }
            parse_chain(stmt, &mut graph);
        }
    }

    if !header_seen || graph.nodes.is_empty() {
        return None;
    }
    graph.dir = dir;
    Some(graph)
}

fn strip_comment(line: &str) -> &str {
    match line.find("%%") {
        Some(i) => &line[..i],
        None => line,
    }
}

fn split_word(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    }
}

/// Statements we knowingly ignore rather than fail on.
fn is_directive(stmt: &str) -> bool {
    let (word, _) = split_word(stmt);
    matches!(
        word,
        "subgraph" | "end" | "style" | "classDef" | "class" | "click" | "linkStyle" | "direction"
    )
}

/// Parses `A[x] --> B{y} -.-> C`, adding every node and link it finds.
fn parse_chain(stmt: &str, graph: &mut Graph) {
    let mut rest = stmt;
    // The link we have read but whose target we have not reached yet.
    let mut pending: Option<(usize, Link, Option<String>)> = None;

    loop {
        let (spec, tail) = split_at_link(rest);
        let Some(current) = parse_node(spec.trim(), graph) else {
            return;
        };
        if let Some((from, link, label)) = pending.take() {
            graph.edges.push(Edge {
                from,
                to: current,
                label,
                link,
            });
        }
        let Some((link, label, after)) = tail else {
            return;
        };
        if after.trim().is_empty() {
            return; // dangling link, e.g. `A -->`
        }
        pending = Some((current, link, label));
        rest = after;
    }
}

/// Splits off the first node spec, returning it plus the link that followed.
fn split_at_link(s: &str) -> (&str, Option<(Link, Option<String>, &str)>) {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    while i < bytes.len() {
        match bytes[i] {
            b'[' | b'(' | b'{' => depth += 1,
            b']' | b')' | b'}' => depth -= 1,
            b'-' | b'=' if depth <= 0 => {
                if let Some((len, link)) = match_link(&s[i..]) {
                    let after = &s[i + len..];
                    let (label, after) = match_label(after);
                    return (&s[..i], Some((link, label, after)));
                }
            }
            _ => {}
        }
        i += 1;
    }
    (s, None)
}

/// Matches a link operator at the start of `s`, returning its byte length.
fn match_link(s: &str) -> Option<(usize, Link)> {
    if let Some(rest) = s.strip_prefix("-.") {
        // Checked in this order: `->` also starts with `-`.
        if rest.starts_with("->") {
            return Some((4, Link::DottedArrow));
        }
        if rest.starts_with('-') {
            return Some((3, Link::DottedLine));
        }
        return None;
    }
    let marker = s.as_bytes().first().copied()?;
    if marker != b'-' && marker != b'=' {
        return None;
    }
    let run = s.bytes().take_while(|&b| b == marker).count();
    if run < 2 {
        return None;
    }
    let arrow = s.as_bytes().get(run) == Some(&b'>');
    match (marker, arrow) {
        (b'-', true) => Some((run + 1, Link::Arrow)),
        (b'=', true) => Some((run + 1, Link::ThickArrow)),
        (b'-', false) if run >= 3 => Some((run, Link::Line)),
        (b'=', false) if run >= 3 => Some((run, Link::ThickLine)),
        _ => None,
    }
}

/// Consumes a `|label|` immediately after a link operator.
fn match_label(s: &str) -> (Option<String>, &str) {
    let trimmed = s.trim_start();
    let Some(body) = trimmed.strip_prefix('|') else {
        return (None, s);
    };
    match body.find('|') {
        Some(end) => (Some(unquote(&body[..end]).to_string()), &body[end + 1..]),
        None => (None, s),
    }
}

/// Parses `id`, `id[label]`, `id{label}` and friends.
fn parse_node(spec: &str, graph: &mut Graph) -> Option<usize> {
    if spec.is_empty() {
        return None;
    }
    let id_len = spec
        .char_indices()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
        .map_or(spec.len(), |(i, _)| i);
    if id_len == 0 {
        return None;
    }
    let (id, rest) = spec.split_at(id_len);
    let idx = graph.node(id);

    let rest = rest.trim();
    if rest.is_empty() {
        return Some(idx);
    }
    // Longest delimiters first, so `([` is not mistaken for `(`.
    const SHAPES: &[(&str, &str, Shape)] = &[
        ("([", "])", Shape::Stadium),
        ("((", "))", Shape::Circle),
        ("{{", "}}", Shape::Hexagon),
        ("[[", "]]", Shape::Subroutine),
        ("[(", ")]", Shape::Cylinder),
        ("[", "]", Shape::Rect),
        ("(", ")", Shape::Round),
        ("{", "}", Shape::Diamond),
    ];
    for (open, close, shape) in SHAPES {
        if let Some(body) = rest.strip_prefix(open).and_then(|b| b.strip_suffix(close)) {
            graph.nodes[idx].label = unquote(body).to_string();
            graph.nodes[idx].shape = *shape;
            return Some(idx);
        }
    }
    Some(idx)
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .unwrap_or(s)
        .trim()
}

// ---------------------------------------------------------------- layout

const FONT_SIZE: f64 = 14.0;
const PAD_X: f64 = 14.0;
const PAD_Y: f64 = 9.0;
const MIN_W: f64 = 44.0;
const NODE_GAP: f64 = 26.0;
const RANK_GAP: f64 = 54.0;
const MARGIN: f64 = 10.0;
const ARROW_LEN: f64 = 9.0;
const ARROW_W: f64 = 7.0;

struct Placed {
    cx: f64,
    cy: f64,
    w: f64,
    h: f64,
}

struct Layout {
    nodes: Vec<Placed>,
    width: f64,
    height: f64,
}

/// The box a node needs, before shape-specific inflation.
fn node_size(node: &Node) -> (f64, f64) {
    let text = text_width(&node.label, FONT_SIZE);
    let w = (text + 2.0 * PAD_X).max(MIN_W);
    let h = FONT_SIZE * 1.4 + 2.0 * PAD_Y;
    match node.shape {
        // A diamond only touches its label at the centre, so it needs slack.
        Shape::Diamond => (w * 1.45 + 16.0, h * 1.8),
        Shape::Circle => {
            let d = w.max(h) * 1.15;
            (d, d)
        }
        Shape::Hexagon => (w + 22.0, h),
        Shape::Subroutine => (w + 16.0, h),
        Shape::Cylinder => (w, h + 14.0),
        _ => (w, h),
    }
}

fn layout(g: &Graph) -> Layout {
    let n = g.nodes.len();
    let sizes: Vec<(f64, f64)> = g.nodes.iter().map(node_size).collect();

    // Rank by longest path over the DAG left once cycle-closing edges are set
    // aside. Without that a loop like A->B->C->A ranks every node further and
    // further down each pass, leaving the diagram full of empty bands.
    let is_back = back_edges(g, n);
    let mut rank = vec![0usize; n];
    for _ in 0..n {
        let mut changed = false;
        for (i, e) in g.edges.iter().enumerate() {
            if !is_back[i] && rank[e.to] < rank[e.from] + 1 {
                rank[e.to] = rank[e.from] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Compact away unused rank numbers so no empty band is laid out.
    let mut used: Vec<usize> = rank.clone();
    used.sort_unstable();
    used.dedup();
    for r in rank.iter_mut() {
        *r = used.binary_search(r).unwrap_or(0);
    }

    let depth = used.len().max(1);
    let mut ranks: Vec<Vec<usize>> = vec![Vec::new(); depth];
    for (i, &r) in rank.iter().enumerate() {
        ranks[r].push(i);
    }

    order_ranks(&mut ranks, g, n);

    // Cross-axis placement: pack each rank, then centre it against the widest.
    let cross_of = |i: usize| if g.dir.vertical() { sizes[i].0 } else { sizes[i].1 };
    let main_of = |i: usize| if g.dir.vertical() { sizes[i].1 } else { sizes[i].0 };

    let mut cross = vec![0.0f64; n];
    let mut extents = Vec::with_capacity(depth);
    for row in &ranks {
        let mut at = 0.0;
        for &i in row {
            cross[i] = at + cross_of(i) / 2.0;
            at += cross_of(i) + NODE_GAP;
        }
        extents.push((at - NODE_GAP).max(0.0));
    }
    let widest = extents.iter().copied().fold(0.0f64, f64::max);
    for (row, extent) in ranks.iter().zip(&extents) {
        let shift = (widest - extent) / 2.0;
        for &i in row {
            cross[i] += shift;
        }
    }

    // Main-axis placement: one band per rank, sized by its tallest member.
    let mut main = vec![0.0f64; n];
    let mut at = 0.0;
    for row in &ranks {
        let band = row.iter().map(|&i| main_of(i)).fold(0.0f64, f64::max);
        for &i in row {
            main[i] = at + band / 2.0;
        }
        at += band + RANK_GAP;
    }
    let total_main = (at - RANK_GAP).max(0.0);

    let nodes = (0..n)
        .map(|i| {
            let (m, c) = (main[i], cross[i]);
            let (cx, cy) = match g.dir {
                Direction::Down => (c, m),
                Direction::Up => (c, total_main - m),
                Direction::Right => (m, c),
                Direction::Left => (total_main - m, c),
            };
            Placed {
                cx: cx + MARGIN,
                cy: cy + MARGIN,
                w: sizes[i].0,
                h: sizes[i].1,
            }
        })
        .collect();

    let (width, height) = if g.dir.vertical() {
        (widest, total_main)
    } else {
        (total_main, widest)
    };
    Layout {
        nodes,
        width: width + 2.0 * MARGIN,
        height: height + 2.0 * MARGIN,
    }
}

/// Flags each edge that closes a cycle, found by an iterative depth-first
/// search. Self-loops count, since a node is always on its own stack.
fn back_edges(g: &Graph, n: usize) -> Vec<bool> {
    let mut succs: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, e) in g.edges.iter().enumerate() {
        succs[e.from].push(i);
    }
    const UNVISITED: u8 = 0;
    const ON_STACK: u8 = 1;
    const DONE: u8 = 2;

    let mut state = vec![UNVISITED; n];
    let mut is_back = vec![false; g.edges.len()];
    for start in 0..n {
        if state[start] != UNVISITED {
            continue;
        }
        state[start] = ON_STACK;
        let mut stack = vec![(start, 0usize)];
        while let Some((v, i)) = stack.pop() {
            let Some(&ei) = succs[v].get(i) else {
                state[v] = DONE;
                continue;
            };
            stack.push((v, i + 1));
            let w = g.edges[ei].to;
            match state[w] {
                ON_STACK => is_back[ei] = true,
                UNVISITED => {
                    state[w] = ON_STACK;
                    stack.push((w, 0));
                }
                _ => {}
            }
        }
    }
    is_back
}

/// Median-heuristic crossing reduction, swept down then up a few times.
fn order_ranks(ranks: &mut [Vec<usize>], g: &Graph, n: usize) {
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut succs: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in &g.edges {
        if e.from != e.to {
            succs[e.from].push(e.to);
            preds[e.to].push(e.from);
        }
    }

    let mut pos = vec![0usize; n];
    for row in ranks.iter() {
        for (i, &v) in row.iter().enumerate() {
            pos[v] = i;
        }
    }

    for pass in 0..4 {
        let downward = pass % 2 == 0;
        let order: Vec<usize> = if downward {
            (1..ranks.len()).collect()
        } else {
            (0..ranks.len().saturating_sub(1)).rev().collect()
        };
        for r in order {
            let mut key = vec![0.0f64; n];
            for &v in &ranks[r] {
                let neighbours = if downward { &preds[v] } else { &succs[v] };
                key[v] = median(neighbours, &pos).unwrap_or(pos[v] as f64);
            }
            ranks[r].sort_by(|a, b| {
                key[*a]
                    .partial_cmp(&key[*b])
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for (i, &v) in ranks[r].iter().enumerate() {
                pos[v] = i;
            }
        }
    }
}

fn median(neighbours: &[usize], pos: &[usize]) -> Option<f64> {
    if neighbours.is_empty() {
        return None;
    }
    let mut xs: Vec<usize> = neighbours.iter().map(|&v| pos[v]).collect();
    xs.sort_unstable();
    let mid = xs.len() / 2;
    Some(if xs.len() % 2 == 1 {
        xs[mid] as f64
    } else {
        (xs[mid - 1] + xs[mid]) as f64 / 2.0
    })
}

/// Advance width of `s`, in px, using Helvetica/Arial metrics (units per em).
///
/// Text cannot be measured without a font engine, so these are the published
/// widths for the first font in the stack. They are close enough that labels
/// sit inside their boxes on every platform.
fn text_width(s: &str, size: f64) -> f64 {
    let units: u32 = s.chars().map(advance).sum();
    f64::from(units) / 1000.0 * size
}

fn advance(c: char) -> u32 {
    match c {
        '\'' => 191,
        'i' | 'j' | 'l' => 222,
        '|' => 260,
        ' ' | '!' | ',' | '.' | ':' | ';' | '/' | '\\' | '[' | ']' | 'I' | 'f' | 't' => 278,
        '(' | ')' | '-' | '`' => 333,
        '{' | '}' => 334,
        '"' => 355,
        '*' => 389,
        '^' => 469,
        'c' | 'k' | 's' | 'v' | 'x' | 'y' | 'z' | 'J' => 500,
        '<' | '=' | '>' | '+' => 584,
        'F' | 'L' | 'T' | 'Z' => 611,
        '&' | 'A' | 'B' | 'E' | 'K' | 'P' | 'S' | 'X' | 'Y' => 667,
        'C' | 'D' | 'H' | 'N' | 'R' | 'U' | 'V' | 'w' => 722,
        'G' | 'O' | 'Q' => 778,
        'M' | 'm' => 833,
        '%' => 889,
        'W' => 944,
        '@' => 1015,
        _ => 556, // digits, remaining lowercase, and anything non-ASCII
    }
}

// ------------------------------------------------------------- rendering

/// Formats a coordinate compactly; SVG does not need full float precision.
fn f(v: f64) -> String {
    let s = format!("{v:.1}");
    s.strip_suffix(".0").unwrap_or(&s).to_string()
}

fn to_svg(l: &Layout, g: &Graph) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(&format!(
        "<svg class=\"mmd\" viewBox=\"0 0 {} {}\" width=\"{}\" height=\"{}\" \
         xmlns=\"http://www.w3.org/2000/svg\" role=\"img\">",
        f(l.width),
        f(l.height),
        f(l.width),
        f(l.height)
    ));
    out.push_str(&format!("<title>{}</title>", escape_html(&summary(g))));

    // Edges first so node fills paint over the line ends.
    for e in &g.edges {
        draw_edge(&mut out, l, g, e);
    }
    for (i, node) in g.nodes.iter().enumerate() {
        draw_node(&mut out, &l.nodes[i], node);
    }
    out.push_str("</svg>");
    out
}

/// Text alternative for screen readers, since the SVG itself is just geometry.
fn summary(g: &Graph) -> String {
    let names: Vec<&str> = g.nodes.iter().map(|n| n.label.as_str()).collect();
    format!(
        "Flowchart with {} nodes and {} connections: {}",
        g.nodes.len(),
        g.edges.len(),
        names.join(", ")
    )
}

fn draw_node(out: &mut String, p: &Placed, node: &Node) {
    let (hw, hh) = (p.w / 2.0, p.h / 2.0);
    let (x, y) = (p.cx - hw, p.cy - hh);
    match node.shape {
        Shape::Circle => out.push_str(&format!(
            "<ellipse class=\"n\" cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\"/>",
            f(p.cx),
            f(p.cy),
            f(hw),
            f(hh)
        )),
        Shape::Diamond => out.push_str(&format!(
            "<polygon class=\"n\" points=\"{},{} {},{} {},{} {},{}\"/>",
            f(p.cx),
            f(y),
            f(x + p.w),
            f(p.cy),
            f(p.cx),
            f(y + p.h),
            f(x),
            f(p.cy)
        )),
        Shape::Hexagon => {
            let d = (p.h / 2.0).min(16.0);
            out.push_str(&format!(
                "<polygon class=\"n\" points=\"{},{} {},{} {},{} {},{} {},{} {},{}\"/>",
                f(x + d),
                f(y),
                f(x + p.w - d),
                f(y),
                f(x + p.w),
                f(p.cy),
                f(x + p.w - d),
                f(y + p.h),
                f(x + d),
                f(y + p.h),
                f(x),
                f(p.cy)
            ));
        }
        Shape::Cylinder => {
            let ry = 7.0;
            // Quadratic curves whose apex lands exactly on the bounding box.
            out.push_str(&format!(
                "<path class=\"n\" d=\"M{},{} Q{},{} {},{} L{},{} Q{},{} {},{} Z\"/>",
                f(x),
                f(y + ry),
                f(p.cx),
                f(y - ry),
                f(x + p.w),
                f(y + ry),
                f(x + p.w),
                f(y + p.h - ry),
                f(p.cx),
                f(y + p.h + ry),
                f(x),
                f(y + p.h - ry)
            ));
            // Front edge of the top cap.
            out.push_str(&format!(
                "<path class=\"e\" d=\"M{},{} Q{},{} {},{}\"/>",
                f(x),
                f(y + ry),
                f(p.cx),
                f(y + 3.0 * ry),
                f(x + p.w),
                f(y + ry)
            ));
        }
        Shape::Subroutine => {
            out.push_str(&format!(
                "<rect class=\"n\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/>",
                f(x),
                f(y),
                f(p.w),
                f(p.h)
            ));
            for dx in [8.0, p.w - 8.0] {
                out.push_str(&format!(
                    "<line class=\"e\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/>",
                    f(x + dx),
                    f(y),
                    f(x + dx),
                    f(y + p.h)
                ));
            }
        }
        shape => {
            let rx = match shape {
                Shape::Stadium => hh,
                Shape::Round => 8.0,
                _ => 3.0,
            };
            out.push_str(&format!(
                "<rect class=\"n\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"{}\"/>",
                f(x),
                f(y),
                f(p.w),
                f(p.h),
                f(rx)
            ));
        }
    }
    out.push_str(&text_at(p.cx, p.cy, &node.label));
}

fn text_at(cx: f64, cy: f64, label: &str) -> String {
    format!(
        "<text x=\"{}\" y=\"{}\" text-anchor=\"middle\" dominant-baseline=\"central\">{}</text>",
        f(cx),
        f(cy),
        escape_html(label)
    )
}

fn draw_edge(out: &mut String, l: &Layout, g: &Graph, e: &Edge) {
    let (a, b) = (&l.nodes[e.from], &l.nodes[e.to]);
    let class = e.link.class();

    if e.from == e.to {
        // Self-loop: a small arc off the top-right corner.
        let (sx, sy) = (a.cx + a.w * 0.25, a.cy - a.h / 2.0);
        let (ex, ey) = (a.cx + a.w / 2.0, a.cy - a.h * 0.25);
        out.push_str(&format!(
            "<path class=\"{}\" d=\"M{} {}C{} {},{} {},{} {}\"/>",
            class,
            f(sx),
            f(sy),
            f(sx),
            f(sy - 26.0),
            f(ex + 26.0),
            f(ey),
            f(ex),
            f(ey)
        ));
        if e.link.has_arrow() {
            out.push_str(&arrow(ex, ey, 1.0, 0.6));
        }
        if let Some(label) = &e.label {
            out.push_str(&edge_label(ex + 18.0, sy - 14.0, label));
        }
        return;
    }

    let (mut dx, mut dy) = (b.cx - a.cx, b.cy - a.cy);
    let mut len = (dx * dx + dy * dy).sqrt();
    if len < f64::EPSILON {
        // Coincident centres: pick an arbitrary direction rather than divide by zero.
        dy = 1.0;
        len = 1.0;
    }
    dx /= len;
    dy /= len;

    let (sx, sy) = boundary(a, g.nodes[e.from].shape, dx, dy);
    let (tx, ty) = boundary(b, g.nodes[e.to].shape, -dx, -dy);
    let (ex, ey) = if e.link.has_arrow() {
        (tx - dx * ARROW_LEN, ty - dy * ARROW_LEN)
    } else {
        (tx, ty)
    };

    out.push_str(&format!(
        "<line class=\"{}\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/>",
        class,
        f(sx),
        f(sy),
        f(ex),
        f(ey)
    ));
    if e.link.has_arrow() {
        out.push_str(&arrow(tx, ty, dx, dy));
    }
    if let Some(label) = &e.label {
        out.push_str(&edge_label((sx + tx) / 2.0, (sy + ty) / 2.0, label));
    }
}

/// A filled triangle with its tip at `(tx, ty)`, pointing along `(dx, dy)`.
fn arrow(tx: f64, ty: f64, dx: f64, dy: f64) -> String {
    let len = (dx * dx + dy * dy).sqrt().max(f64::EPSILON);
    let (dx, dy) = (dx / len, dy / len);
    let (bx, by) = (tx - dx * ARROW_LEN, ty - dy * ARROW_LEN);
    let (px, py) = (-dy * ARROW_W / 2.0, dx * ARROW_W / 2.0);
    format!(
        "<polygon class=\"a\" points=\"{},{} {},{} {},{}\"/>",
        f(tx),
        f(ty),
        f(bx + px),
        f(by + py),
        f(bx - px),
        f(by - py)
    )
}

/// Where a ray leaving the node centre along `(dx, dy)` crosses its outline.
fn boundary(p: &Placed, shape: Shape, dx: f64, dy: f64) -> (f64, f64) {
    let (hw, hh) = (p.w / 2.0, p.h / 2.0);
    let t = match shape {
        Shape::Circle => {
            let (a, b) = (dx / hw, dy / hh);
            1.0 / (a * a + b * b).sqrt()
        }
        Shape::Diamond => 1.0 / (dx.abs() / hw + dy.abs() / hh),
        _ => {
            let tx = if dx.abs() > f64::EPSILON {
                hw / dx.abs()
            } else {
                f64::MAX
            };
            let ty = if dy.abs() > f64::EPSILON {
                hh / dy.abs()
            } else {
                f64::MAX
            };
            tx.min(ty)
        }
    };
    (p.cx + dx * t, p.cy + dy * t)
}

fn edge_label(cx: f64, cy: f64, label: &str) -> String {
    let w = text_width(label, FONT_SIZE) + 8.0;
    let h = FONT_SIZE + 6.0;
    format!(
        "<rect class=\"lb\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"3\"/>{}",
        f(cx - w / 2.0),
        f(cy - h / 2.0),
        f(w),
        f(h),
        text_at(cx, cy, label)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Set;

    fn render(src: &str) -> String {
        let set = Set::resolve(&["mermaid".to_string()]).unwrap();
        set.render_html(src).html
    }

    fn fence(body: &str) -> String {
        format!("```mermaid\n{body}\n```\n")
    }

    fn svg(body: &str) -> String {
        render(&fence(body))
    }

    #[test]
    fn renders_a_basic_flowchart() {
        let out = svg("flowchart TD\n  A[Start] --> B[End]");
        assert!(out.contains("<svg"), "{out}");
        assert!(out.contains(">Start<"), "{out}");
        assert!(out.contains(">End<"), "{out}");
        assert!(out.contains("<polygon class=\"a\""), "arrowhead missing: {out}");
        // No code block survives, and nothing executable is emitted.
        assert!(!out.contains("language-mermaid"), "{out}");
        assert!(!out.contains("<script"), "{out}");
    }

    #[test]
    fn graph_keyword_is_accepted() {
        assert!(svg("graph LR\n  A --> B").contains("<svg"));
    }

    #[test]
    fn undeclared_nodes_are_created_on_first_mention() {
        let out = svg("flowchart TD\n  A --> B --> C");
        for label in [">A<", ">B<", ">C<"] {
            assert!(out.contains(label), "missing {label}: {out}");
        }
        // A chain of three nodes is two edges.
        assert_eq!(out.matches("<line class=").count(), 2, "{out}");
    }

    #[test]
    fn later_declaration_updates_an_earlier_mention() {
        let out = svg("flowchart TD\n  A --> B\n  B[Renamed]");
        assert!(out.contains(">Renamed<"), "{out}");
    }

    #[test]
    fn node_shapes_map_to_svg_elements() {
        assert!(svg("flowchart TD\n  A{Choice}").contains("<polygon class=\"n\""));
        assert!(svg("flowchart TD\n  A((Round))").contains("<ellipse class=\"n\""));
        assert!(svg("flowchart TD\n  A([Stad])").contains("<rect class=\"n\""));
        assert!(svg("flowchart TD\n  A{{Hex}}").contains("<polygon class=\"n\""));
        assert!(svg("flowchart TD\n  A[(Data)]").contains("<path class=\"n\""));
    }

    #[test]
    fn link_styles_get_distinct_classes() {
        assert!(svg("flowchart TD\n  A -.-> B").contains("class=\"e dash\""));
        assert!(svg("flowchart TD\n  A ==> B").contains("class=\"e thick\""));
        // A plain line has no arrowhead.
        let plain = svg("flowchart TD\n  A --- B");
        assert!(plain.contains("<line class=\"e\""), "{plain}");
        assert!(!plain.contains("<polygon class=\"a\""), "{plain}");
    }

    #[test]
    fn edge_labels_are_rendered() {
        let out = svg("flowchart TD\n  A -->|Yes| B\n  A -->|No| C");
        assert!(out.contains(">Yes<"), "{out}");
        assert!(out.contains(">No<"), "{out}");
        assert!(out.contains("<rect class=\"lb\""), "{out}");
    }

    #[test]
    fn all_directions_parse_and_swap_the_axes() {
        let td = svg("flowchart TD\n  A[Wide label here] --> B");
        let lr = svg("flowchart LR\n  A[Wide label here] --> B");
        assert!(td.contains("<svg") && lr.contains("<svg"));
        // Stacking vertically vs horizontally must not produce the same canvas.
        assert_ne!(viewbox(&td), viewbox(&lr), "TD and LR laid out identically");
        for dir in ["TB", "BT", "RL"] {
            assert!(svg(&format!("flowchart {dir}\n  A --> B")).contains("<svg"));
        }
    }

    fn viewbox(svg: &str) -> String {
        let start = svg.find("viewBox=\"").unwrap() + 9;
        svg[start..].split('"').next().unwrap().to_string()
    }

    /// The canvas width from the viewBox, as a number.
    fn canvas_width(svg: &str) -> f64 {
        viewbox(svg).split_whitespace().nth(2).unwrap().parse().unwrap()
    }

    #[test]
    fn comments_and_directives_are_skipped() {
        let out = svg("flowchart TD\n  %% a comment\n  A --> B\n  style A fill:#f00\n  class A x");
        assert!(out.contains("<svg"), "{out}");
        assert!(!out.contains("comment"), "{out}");
        assert!(!out.contains("fill:#f00"), "{out}");
    }

    #[test]
    fn semicolons_separate_statements() {
        let out = svg("flowchart TD\n  A-->B; B-->C");
        assert_eq!(out.matches("<line class=").count(), 2, "{out}");
    }

    #[test]
    fn other_diagram_types_stay_code_blocks() {
        for body in ["sequenceDiagram\n  A->>B: hi", "classDiagram\n  class A", "pie\n  \"a\": 1"] {
            let out = svg(body);
            assert!(!out.contains("<svg"), "should not render: {out}");
            assert!(out.contains("language-mermaid"), "{out}");
        }
    }

    #[test]
    fn unparseable_input_stays_a_code_block() {
        let out = svg("not a diagram at all");
        assert!(!out.contains("<svg"), "{out}");
        assert!(out.contains("language-mermaid"), "{out}");
    }

    #[test]
    fn cycles_terminate() {
        // Longest-path ranking must not loop on a cycle.
        let out = svg("flowchart TD\n  A --> B\n  B --> C\n  C --> A");
        assert!(out.contains("<svg"), "{out}");
        assert_eq!(out.matches("<line class=").count(), 3, "{out}");
    }

    #[test]
    fn self_loops_are_drawn_not_dropped() {
        let out = svg("flowchart TD\n  A --> A");
        assert!(out.contains("<path class=\"e\""), "{out}");
    }

    #[test]
    fn labels_cannot_inject_markup() {
        // The SVG goes into a `Raw` node, so every label must be escaped here.
        let out = svg("flowchart TD\n  A[<script>alert(1)</script>] -->|<img src=x>| B");
        assert!(!out.contains("<script"), "{out}");
        assert!(!out.contains("<img"), "{out}");
        assert!(out.contains("&lt;script&gt;"), "{out}");
    }

    #[test]
    fn quoted_labels_are_unwrapped() {
        let out = svg("flowchart TD\n  A[\"Hello, world\"] --> B");
        assert!(out.contains(">Hello, world<"), "{out}");
        assert!(!out.contains("&quot;"), "quotes should be stripped, not escaped: {out}");
    }

    #[test]
    fn disabled_without_the_plugin() {
        let out = Set::default().render_html(&fence("flowchart TD\n  A --> B")).html;
        assert!(!out.contains("<svg"), "{out}");
        assert!(out.contains("language-mermaid"), "{out}");
    }

    #[test]
    fn text_width_tracks_label_length() {
        assert!(text_width("WWWW", FONT_SIZE) > text_width("iiii", FONT_SIZE));
        assert_eq!(text_width("", FONT_SIZE), 0.0);
    }

    #[test]
    fn wider_labels_produce_wider_nodes() {
        let narrow = canvas_width(&svg("flowchart TD\n  A[Hi] --> B"));
        let wide = canvas_width(&svg("flowchart TD\n  A[A considerably longer label] --> B"));
        assert!(wide > narrow, "{wide} should exceed {narrow}");
    }

    #[test]
    fn head_style_ships_only_when_a_diagram_rendered() {
        let set = Set::resolve(&["mermaid".to_string()]).unwrap();
        assert!(set.render_html("just prose\n").head.is_empty());
        assert!(!set
            .render_html(&fence("flowchart TD\n  A --> B"))
            .head
            .is_empty());
    }
}

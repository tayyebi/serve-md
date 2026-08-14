use comrak::nodes::{AstNode, ListDelimType, ListType, NodeList, NodeTable, NodeValue, TableAlignment};
use comrak::{parse_document, Arena, Options};

const WIDTH: usize = 80;

/// Renders Markdown source to reader-friendly plain-text ASCII, suitable for
/// terminal clients (curl, wget) that can't render HTML.
pub fn markdown_to_ascii(src: &str) -> String {
    let arena = Arena::new();
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.tagfilter = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    let root = parse_document(&arena, src, &options);

    let mut out = render_blocks(root, WIDTH).join("\n");
    out.push('\n');
    out
}

fn render_blocks<'a>(node: &'a AstNode<'a>, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut first = true;
    for child in node.children() {
        let child_lines = render_block(child, width);
        if child_lines.is_empty() {
            continue;
        }
        if !first {
            lines.push(String::new());
        }
        first = false;
        lines.extend(child_lines);
    }
    lines
}

fn render_block<'a>(node: &'a AstNode<'a>, width: usize) -> Vec<String> {
    let value = node.data.borrow().value.clone();
    match value {
        NodeValue::Paragraph => wrap_text(&render_inline(node), width),
        NodeValue::Heading(h) => render_heading(node, h.level, width),
        NodeValue::BlockQuote => prefix_all(render_blocks(node, width.saturating_sub(2)), "> "),
        NodeValue::List(list) => render_list(node, &list, width),
        NodeValue::CodeBlock(cb) => render_code_block(&cb.literal),
        NodeValue::ThematicBreak => vec!["-".repeat(width.clamp(3, 72))],
        NodeValue::Table(tbl) => render_table(node, &tbl, width),
        NodeValue::HtmlBlock(hb) => wrap_text(&strip_tags(&hb.literal), width),
        _ => render_blocks(node, width),
    }
}

fn render_heading<'a>(node: &'a AstNode<'a>, level: u8, width: usize) -> Vec<String> {
    let text = render_inline(node);
    match level {
        1 => {
            let wrapped = wrap_text(&text.to_uppercase(), width);
            underline(wrapped, '=')
        }
        2 => {
            let wrapped = wrap_text(&text, width);
            underline(wrapped, '-')
        }
        _ => {
            let marker = format!("{} ", "#".repeat(level as usize));
            let hang = " ".repeat(marker.chars().count());
            let wrapped = wrap_text(&text, width.saturating_sub(marker.chars().count()).max(10));
            prefix_first_rest(wrapped, &marker, &hang)
        }
    }
}

fn underline(mut lines: Vec<String>, ch: char) -> Vec<String> {
    let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0).max(1);
    lines.push(ch.to_string().repeat(width));
    lines
}

fn render_list<'a>(node: &'a AstNode<'a>, list: &NodeList, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut num = list.start;
    let mut first = true;
    for item in node.children() {
        let item_value = item.data.borrow().value.clone();
        let is_task = matches!(item_value, NodeValue::TaskItem(_));
        let marker = match item_value {
            NodeValue::TaskItem(nti) => {
                let checked = if nti.symbol.is_some() { "x" } else { " " };
                format!("- [{checked}] ")
            }
            _ => match list.list_type {
                ListType::Bullet => "- ".to_string(),
                ListType::Ordered => {
                    let delim = match list.delimiter {
                        ListDelimType::Period => ".",
                        ListDelimType::Paren => ")",
                    };
                    format!("{num}{delim} ")
                }
            },
        };
        if !is_task && matches!(list.list_type, ListType::Ordered) {
            num += 1;
        }
        let hang = " ".repeat(marker.chars().count());
        let item_width = width.saturating_sub(hang.chars().count()).max(10);
        let body = render_blocks(item, item_width);
        let decorated = prefix_first_rest(body, &marker, &hang);

        if !first && !list.tight {
            lines.push(String::new());
        }
        first = false;
        lines.extend(decorated);
    }
    lines
}

fn render_code_block(literal: &str) -> Vec<String> {
    let mut lines: Vec<String> = literal.lines().map(|l| format!("    {l}")).collect();
    if lines.is_empty() {
        lines.push("    ".to_string());
    }
    lines
}

fn render_table<'a>(node: &'a AstNode<'a>, tbl: &NodeTable, width: usize) -> Vec<String> {
    let num_cols = tbl.num_columns;
    let mut rows: Vec<(bool, Vec<String>)> = Vec::new();
    for row_node in node.children() {
        let is_header = matches!(row_node.data.borrow().value, NodeValue::TableRow(true));
        let mut cells: Vec<String> = Vec::new();
        for cell_node in row_node.children() {
            cells.push(render_inline(cell_node));
        }
        rows.push((is_header, cells));
    }

    let max_cell_width = (width.saturating_sub(num_cols * 3 + 1) / num_cols.max(1)).clamp(6, 30);
    let mut col_w = vec![0usize; num_cols];
    for (_, cells) in &rows {
        for (i, c) in cells.iter().enumerate() {
            if i < num_cols {
                col_w[i] = col_w[i].max(c.chars().count().min(max_cell_width));
            }
        }
    }
    for w in col_w.iter_mut() {
        *w = (*w).max(3);
    }

    let sep = build_separator(&col_w);
    let mut lines = vec![sep.clone()];
    for (is_header, cells) in &rows {
        let wrapped_cells: Vec<Vec<String>> = col_w
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let text = cells.get(i).map(|s| s.as_str()).unwrap_or("");
                wrap_text(text, *w)
            })
            .collect();
        let height = wrapped_cells.iter().map(|c| c.len()).max().unwrap_or(1);
        for line_i in 0..height {
            let mut row_line = String::from("|");
            for (i, w) in col_w.iter().enumerate() {
                let text = wrapped_cells[i].get(line_i).map(|s| s.as_str()).unwrap_or("");
                let align = tbl.alignments.get(i).copied().unwrap_or(TableAlignment::None);
                row_line.push(' ');
                row_line.push_str(&pad(text, *w, align));
                row_line.push_str(" |");
            }
            lines.push(row_line);
        }
        if *is_header {
            lines.push(sep.clone());
        }
    }
    lines.push(sep);
    lines
}

fn build_separator(col_w: &[usize]) -> String {
    let mut s = String::from("+");
    for w in col_w {
        s.push_str(&"-".repeat(w + 2));
        s.push('+');
    }
    s
}

fn pad(text: &str, width: usize, align: TableAlignment) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_string();
    }
    let gap = width - len;
    match align {
        TableAlignment::Right => format!("{}{text}", " ".repeat(gap)),
        TableAlignment::Center => {
            let left = gap / 2;
            let right = gap - left;
            format!("{}{text}{}", " ".repeat(left), " ".repeat(right))
        }
        TableAlignment::Left | TableAlignment::None => format!("{text}{}", " ".repeat(gap)),
    }
}

fn render_inline<'a>(node: &'a AstNode<'a>) -> String {
    let mut s = String::new();
    render_inline_into(node, &mut s);
    s
}

fn render_inline_into<'a>(node: &'a AstNode<'a>, out: &mut String) {
    for child in node.children() {
        let value = child.data.borrow().value.clone();
        match value {
            NodeValue::Text(t) => out.push_str(&t),
            NodeValue::Code(c) => {
                out.push('`');
                out.push_str(&c.literal);
                out.push('`');
            }
            NodeValue::Emph => {
                out.push('*');
                render_inline_into(child, out);
                out.push('*');
            }
            NodeValue::Strong => {
                out.push_str("**");
                render_inline_into(child, out);
                out.push_str("**");
            }
            NodeValue::Strikethrough => {
                out.push_str("~~");
                render_inline_into(child, out);
                out.push_str("~~");
            }
            NodeValue::SoftBreak | NodeValue::LineBreak => out.push(' '),
            NodeValue::Link(link) => {
                let mut inner = String::new();
                render_inline_into(child, &mut inner);
                let same = inner.trim() == link.url.trim();
                out.push_str(&inner);
                if !same {
                    out.push_str(" (");
                    out.push_str(&link.url);
                    out.push(')');
                }
            }
            NodeValue::Image(link) => {
                let mut alt = String::new();
                render_inline_into(child, &mut alt);
                if alt.trim().is_empty() {
                    out.push_str("[image]");
                } else {
                    out.push_str("[image: ");
                    out.push_str(alt.trim());
                    out.push(']');
                }
                out.push_str(" (");
                out.push_str(&link.url);
                out.push(')');
            }
            NodeValue::HtmlInline(html) => out.push_str(&strip_tags(&html)),
            _ => render_inline_into(child, out),
        }
    }
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn prefix_all(lines: Vec<String>, prefix: &str) -> Vec<String> {
    lines
        .into_iter()
        .map(|l| if l.is_empty() { l } else { format!("{prefix}{l}") })
        .collect()
}

fn prefix_first_rest(lines: Vec<String>, first: &str, rest: &str) -> Vec<String> {
    if lines.is_empty() {
        return vec![first.trim_end().to_string()];
    }
    lines
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            if i > 0 && l.is_empty() {
                l
            } else {
                format!("{}{l}", if i == 0 { first } else { rest })
            }
        })
        .collect()
}

fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_len = 0usize;
    for word in s.split_whitespace() {
        let wlen = word.chars().count();
        if line.is_empty() {
            line.push_str(word);
            line_len = wlen;
        } else if line_len + 1 + wlen <= width {
            line.push(' ');
            line.push_str(word);
            line_len += 1 + wlen;
        } else {
            lines.push(std::mem::take(&mut line));
            line.push_str(word);
            line_len = wlen;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_headings() {
        let out = markdown_to_ascii("# Title\n\n## Sub\n");
        assert!(out.contains("TITLE"));
        assert!(out.contains("====="));
        assert!(out.contains("Sub"));
        assert!(out.contains("---"));
    }

    #[test]
    fn renders_paragraph_and_emphasis() {
        let out = markdown_to_ascii("Hello **world**, this is *ascii*.\n");
        assert!(out.contains("**world**"));
        assert!(out.contains("*ascii*"));
    }

    #[test]
    fn renders_unordered_list() {
        let out = markdown_to_ascii("- one\n- two\n  - nested\n");
        assert!(out.contains("- one"));
        assert!(out.contains("- two"));
        assert!(out.contains("- nested"));
    }

    #[test]
    fn renders_ordered_list() {
        let out = markdown_to_ascii("1. first\n2. second\n");
        assert!(out.contains("1. first"));
        assert!(out.contains("2. second"));
    }

    #[test]
    fn renders_task_list() {
        let out = markdown_to_ascii("- [ ] todo\n- [x] done\n");
        assert!(out.contains("- [ ] todo"));
        assert!(out.contains("- [x] done"));
    }

    #[test]
    fn renders_table() {
        let out = markdown_to_ascii("| A | B |\n|---|---|\n| 1 | 2 |\n");
        assert!(out.contains('+'));
        assert!(out.contains("| A"));
        assert!(out.contains("| 1"));
    }

    #[test]
    fn renders_blockquote() {
        let out = markdown_to_ascii("> quoted text\n");
        assert!(out.contains("> quoted text"));
    }

    #[test]
    fn renders_code_block() {
        let out = markdown_to_ascii("```\nfn main() {}\n```\n");
        assert!(out.contains("    fn main() {}"));
    }

    #[test]
    fn renders_link() {
        let out = markdown_to_ascii("[docs](https://example.com)\n");
        assert!(out.contains("docs (https://example.com)"));
    }

    #[test]
    fn wraps_long_paragraphs() {
        let long = "word ".repeat(40);
        let out = markdown_to_ascii(&long);
        assert!(out.lines().all(|l| l.chars().count() <= WIDTH));
    }
}

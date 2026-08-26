use comrak::nodes::{AstNode, ListDelimType, ListType, NodeList, NodeTable, NodeValue, TableAlignment};
use comrak::{parse_document, Arena, Options};

const WIDTH: usize = 80;

/// Renders Markdown source to reader-friendly plain-text ASCII, suitable for
/// terminal clients (curl, wget) that can't render HTML.
///
/// `options` comes from the active plugin set, so this sees the same AST the
/// HTML renderer does. Plugin AST transforms are *not* applied here — they
/// produce HTML, which is meaningless in a terminal.
pub fn markdown_to_ascii(src: &str, options: &Options<'_>) -> String {
    let arena = Arena::new();
    let root = parse_document(&arena, src, options);

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
            // Math nodes have no children, so without this arm the generic
            // fallback below would recurse into nothing and drop the formula.
            // The delimiters are kept so the output stays unambiguous and can
            // be pasted straight back into a .md file.
            NodeValue::Math(m) => {
                let (open, close) = match (m.dollar_math, m.display_math) {
                    (true, true) => ("$$", "$$"),
                    (true, false) => ("$", "$"),
                    (false, _) => ("$`", "`$"),
                };
                out.push_str(open);
                out.push_str(m.literal.trim());
                out.push_str(close);
            }
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
    use crate::plugin::Set;

    /// Renders with no plugins active — the default configuration.
    fn ascii(src: &str) -> String {
        markdown_to_ascii(src, &Set::default().options())
    }

    /// Renders with the math plugin's parser extensions on.
    fn ascii_math(src: &str) -> String {
        let set = Set::resolve(&["math".to_string()]).unwrap();
        markdown_to_ascii(src, &set.options())
    }

    #[test]
    fn renders_headings() {
        let out = ascii("# Title\n\n## Sub\n");
        assert!(out.contains("TITLE"));
        assert!(out.contains("====="));
        assert!(out.contains("Sub"));
        assert!(out.contains("---"));
    }

    #[test]
    fn renders_paragraph_and_emphasis() {
        let out = ascii("Hello **world**, this is *ascii*.\n");
        assert!(out.contains("**world**"));
        assert!(out.contains("*ascii*"));
    }

    #[test]
    fn renders_unordered_list() {
        let out = ascii("- one\n- two\n  - nested\n");
        assert!(out.contains("- one"));
        assert!(out.contains("- two"));
        assert!(out.contains("- nested"));
    }

    #[test]
    fn renders_ordered_list() {
        let out = ascii("1. first\n2. second\n");
        assert!(out.contains("1. first"));
        assert!(out.contains("2. second"));
    }

    #[test]
    fn renders_task_list() {
        let out = ascii("- [ ] todo\n- [x] done\n");
        assert!(out.contains("- [ ] todo"));
        assert!(out.contains("- [x] done"));
    }

    #[test]
    fn renders_table() {
        let out = ascii("| A | B |\n|---|---|\n| 1 | 2 |\n");
        assert!(out.contains('+'));
        assert!(out.contains("| A"));
        assert!(out.contains("| 1"));
    }

    #[test]
    fn renders_blockquote() {
        let out = ascii("> quoted text\n");
        assert!(out.contains("> quoted text"));
    }

    #[test]
    fn renders_code_block() {
        let out = ascii("```\nfn main() {}\n```\n");
        assert!(out.contains("    fn main() {}"));
    }

    #[test]
    fn renders_link() {
        let out = ascii("[docs](https://example.com)\n");
        assert!(out.contains("docs (https://example.com)"));
    }

    #[test]
    fn wraps_long_paragraphs() {
        let long = "word ".repeat(40);
        let out = ascii(&long);
        assert!(out.lines().all(|l| l.chars().count() <= WIDTH));
    }

    #[test]
    fn math_keeps_its_delimiters() {
        let out = ascii_math("mass energy $E = mc^2$ here\n");
        assert!(out.contains("$E = mc^2$"), "{out}");
    }

    #[test]
    fn display_math_keeps_double_delimiters() {
        let out = ascii_math("$$x^2 + y^2$$\n");
        assert!(out.contains("$$x^2 + y^2$$"), "{out}");
    }

    #[test]
    fn code_math_keeps_backtick_delimiters() {
        let out = ascii_math("inline $`a + b`$ here\n");
        assert!(out.contains("$`a + b`$"), "{out}");
    }

    #[test]
    fn math_is_plain_text_without_the_plugin() {
        let out = ascii("mass energy $E = mc^2$ here\n");
        assert!(out.contains("$E = mc^2$"), "{out}");
    }

    #[test]
    fn renders_inline_code_and_strikethrough() {
        let out = ascii("use `cargo test`, not ~~make~~.\n");
        assert!(out.contains("`cargo test`"), "{out}");
        assert!(out.contains("~~make~~"), "{out}");
    }

    #[test]
    fn renders_images_with_alt_text() {
        assert!(ascii("![Alt](a.png)\n").contains("[image: Alt] (a.png)"));
        assert!(ascii("![](a.png)\n").contains("[image] (a.png)"));
    }

    #[test]
    fn autolinked_url_is_not_repeated() {
        let out = ascii("see https://example.com now\n");
        assert_eq!(out.matches("https://example.com").count(), 1, "{out}");
    }

    #[test]
    fn renders_thematic_break_as_a_rule() {
        let out = ascii("a\n\n---\n\nb\n");
        assert!(out.lines().any(|l| l.chars().all(|c| c == '-') && l.len() >= 3), "{out}");
    }

    #[test]
    fn ordered_list_honours_start_and_delimiter() {
        assert!(ascii("5. five\n6. six\n").contains("5. five"));
        assert!(ascii("1) first\n").contains("1) first"));
    }

    #[test]
    fn nested_blockquotes_nest_their_markers() {
        let out = ascii("> outer\n>\n> > inner\n");
        assert!(out.contains("> outer"), "{out}");
        assert!(out.contains("> > inner"), "{out}");
    }

    #[test]
    fn html_blocks_are_stripped_to_their_text() {
        let out = ascii("<div class=\"x\">hello</div>\n");
        assert!(out.contains("hello"), "{out}");
        assert!(!out.contains('<'), "{out}");
        assert!(!out.contains("class"), "{out}");
    }

    #[test]
    fn soft_breaks_join_into_one_paragraph() {
        let out = ascii("one\ntwo\n");
        assert!(out.contains("one two"), "{out}");
    }

    #[test]
    fn code_blocks_keep_their_inner_indentation() {
        let out = ascii("```\nif x:\n    y()\n```\n");
        assert!(out.contains("    if x:"), "{out}");
        assert!(out.contains("        y()"), "{out}");
    }

    #[test]
    fn table_columns_align_per_the_delimiter_row() {
        let out = ascii("| L | R |\n|:--|--:|\n| a | b |\n");
        assert!(out.contains("| a  "), "{out}");
        assert!(out.contains("  b |"), "{out}");
    }

    #[test]
    fn empty_input_renders_a_single_newline() {
        assert_eq!(ascii(""), "\n");
    }

    // ------------------------------------------------ markdown -> txt edges

    #[test]
    fn heading_underline_matches_the_longest_line() {
        let out = ascii("# Title\n");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "TITLE");
        assert_eq!(lines[1], "=====");
        let sub = ascii("## Sub\n");
        assert_eq!(sub.lines().nth(1), Some("---"));
    }

    #[test]
    fn deep_headings_use_hash_markers() {
        assert!(ascii("### Third\n").contains("### Third"));
        assert!(ascii("###### Sixth\n").contains("###### Sixth"));
        // Level 3+ is not underlined.
        assert!(!ascii("### Third\n").contains("==="));
    }

    #[test]
    fn setext_and_atx_headings_render_the_same() {
        assert_eq!(ascii("Title\n=====\n"), ascii("# Title\n"));
    }

    #[test]
    fn thematic_break_is_clamped_to_seventy_two_dashes() {
        let out = ascii("a\n\n---\n\nb\n");
        assert!(out.lines().any(|l| l == "-".repeat(72)), "{out}");
    }

    #[test]
    fn nested_lists_are_indented_under_their_parent() {
        let out = ascii("- one\n  - inner\n    - deepest\n");
        assert!(out.contains("- one"), "{out}");
        assert!(out.contains("  - inner"), "{out}");
        assert!(out.contains("    - deepest"), "{out}");
    }

    #[test]
    fn ordered_list_continuation_lines_hang_under_the_marker() {
        let long = "x".repeat(20) + " " + &"y".repeat(20) + " " + &"z".repeat(60);
        let out = ascii(&format!("1. {long}\n"));
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("1. "), "{out}");
        assert!(lines.len() > 1, "should wrap: {out}");
        assert!(lines[1].starts_with("   ") && !lines[1].starts_with("    "), "{out}");
    }

    #[test]
    fn loose_lists_get_a_blank_line_between_items() {
        let loose = ascii("- a\n\n- b\n");
        assert!(loose.contains("- a\n\n- b"), "{loose:?}");
        let tight = ascii("- a\n- b\n");
        assert!(tight.contains("- a\n- b"), "{tight:?}");
    }

    #[test]
    fn a_list_inside_a_blockquote_keeps_both_markers() {
        let out = ascii("> - a\n> - b\n");
        assert!(out.contains("> - a"), "{out}");
        assert!(out.contains("> - b"), "{out}");
    }

    #[test]
    fn blockquotes_wrap_inside_their_narrowed_width() {
        let out = ascii(&format!("> {}\n", "word ".repeat(40)));
        assert!(out.lines().all(|l| l.starts_with("> ")), "{out}");
        assert!(out.lines().all(|l| l.chars().count() <= WIDTH), "{out}");
    }

    #[test]
    fn code_blocks_are_never_wrapped() {
        let long = "x".repeat(200);
        let out = ascii(&format!("```\n{long}\n```\n"));
        assert!(out.contains(&format!("    {long}")), "{out}");
    }

    #[test]
    fn a_ragged_table_row_is_padded_out() {
        let out = ascii("| A | B |\n|---|---|\n| 1 |\n");
        // Every rendered row spans both columns.
        for line in out.lines().filter(|l| l.starts_with('|')) {
            assert_eq!(line.matches('|').count(), 3, "{out}");
        }
    }

    #[test]
    fn wide_table_cells_wrap_rather_than_overflow() {
        let cell = "long ".repeat(20);
        let out = ascii(&format!("| A | B |\n|---|---|\n| {cell} | 2 |\n"));
        assert!(out.lines().all(|l| l.chars().count() <= WIDTH + 4), "{out}");
        assert!(out.lines().filter(|l| l.starts_with('|')).count() > 2, "{out}");
    }

    #[test]
    fn hard_breaks_become_spaces() {
        assert!(ascii("one  \ntwo\n").contains("one two"));
        assert!(ascii("one\\\ntwo\n").contains("one two"));
    }

    #[test]
    fn nested_emphasis_keeps_both_markers() {
        assert!(ascii("***both***\n").contains("***both***"));
    }

    #[test]
    fn an_image_inside_a_link_renders_both_targets() {
        let out = ascii("[![Alt](i.png)](https://example.com)\n");
        assert!(out.contains("[image: Alt] (i.png)"), "{out}");
        assert!(out.contains("(https://example.com)"), "{out}");
    }

    #[test]
    fn inline_html_is_stripped_but_its_text_kept() {
        let out = ascii("text <b>bold</b> here\n");
        assert!(out.contains("text bold here"), "{out}");
        assert!(!out.contains('<'), "{out}");
    }

    #[test]
    fn non_ascii_text_survives_intact() {
        let out = ascii("caf\u{e9} \u{2014} \u{6f22}\u{5b57} \u{1f600}\n");
        assert!(out.contains("caf\u{e9}"), "{out}");
        assert!(out.contains("\u{6f22}\u{5b57}"), "{out}");
        assert!(out.contains("\u{1f600}"), "{out}");
    }

    #[test]
    fn wrapping_counts_characters_not_bytes() {
        // Multi-byte words must not make lines wrap early.
        let out = ascii(&"caf\u{e9} ".repeat(30));
        let widest = out.lines().map(|l| l.chars().count()).max().unwrap();
        assert!(widest <= WIDTH, "{widest} > {WIDTH}");
        assert!(widest > WIDTH - 6, "wrapped too early at {widest}: {out}");
    }

    #[test]
    fn an_over_long_word_is_left_whole() {
        let word = "x".repeat(120);
        let out = ascii(&format!("{word}\n"));
        assert!(out.contains(&word), "a word must never be split: {out}");
    }

    #[test]
    fn blank_and_whitespace_only_documents_render_cleanly() {
        assert_eq!(ascii("   \n\n  \n"), "\n");
        assert_eq!(ascii("\n\n\n"), "\n");
    }

    #[test]
    fn output_never_contains_tabs_or_trailing_blank_lines() {
        let out = ascii("# Title\n\nBody with a [link](https://example.com).\n");
        assert!(!out.contains('\t'), "{out}");
        assert!(out.ends_with('\n') && !out.ends_with("\n\n"), "{out:?}");
    }
}

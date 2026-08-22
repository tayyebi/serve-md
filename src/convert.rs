//! HTML -> Markdown / plain-text conversion, used to satisfy content
//! negotiation (`Accept: text/markdown` / `text/plain`) for served `.html`
//! files.

#[derive(Debug)]
enum Token<'a> {
    Text(&'a str),
    Tag {
        name: String,
        closing: bool,
        self_closing: bool,
        attrs: Vec<(String, String)>,
    },
}

fn tokenize(html: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let bytes = html.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(end) = html[i..].find('>') {
                let end = i + end;
                let inner = &html[i + 1..end];
                if let Some(stripped) = inner.strip_prefix('!') {
                    let _ = stripped;
                    i = end + 1;
                    continue;
                }
                let closing = inner.starts_with('/');
                let body = inner.strip_prefix('/').unwrap_or(inner).trim();
                let self_closing = body.ends_with('/');
                let body = body.strip_suffix('/').unwrap_or(body).trim();
                let mut parts = body.splitn(2, char::is_whitespace);
                let name = parts.next().unwrap_or("").to_ascii_lowercase();
                let attrs = parse_attrs(parts.next().unwrap_or(""));
                if !name.is_empty() {
                    out.push(Token::Tag {
                        name,
                        closing,
                        self_closing,
                        attrs,
                    });
                }
                i = end + 1;
                continue;
            } else {
                out.push(Token::Text(&html[i..]));
                break;
            }
        }
        let next = html[i..].find('<').map(|p| i + p).unwrap_or(html.len());
        if next > i {
            out.push(Token::Text(&html[i..next]));
        }
        i = next;
    }
    out
}

fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i == name_start {
            break;
        }
        let name = s[name_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let val_start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                out.push((name, s[val_start..i].to_string()));
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                let val_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                out.push((name, s[val_start..i].to_string()));
            }
        } else {
            out.push((name, String::new()));
        }
    }
    out
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out
}

/// Converts an HTML document to Markdown source.
pub fn html_to_markdown(html: &str) -> String {
    let mut out = String::new();
    let mut skip_depth = 0u32; // inside <script>/<style>/<head>
    let mut list_stack: Vec<char> = Vec::new(); // '-' or '1'
    let mut pre_depth = 0u32;
    let mut pending_href: Vec<Option<String>> = Vec::new();

    let ensure_blank_line = |out: &mut String| {
        if !out.is_empty() && !out.ends_with("\n\n") {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
    };

    for tok in tokenize(html) {
        match tok {
            Token::Text(t) => {
                if skip_depth > 0 {
                    continue;
                }
                if pre_depth > 0 {
                    out.push_str(&decode_entities(t));
                    continue;
                }
                let text = collapse_ws(&decode_entities(t));
                if text.trim().is_empty() {
                    continue;
                }
                out.push_str(text.trim_start());
            }
            Token::Tag {
                name,
                closing,
                self_closing,
                attrs,
            } => {
                if matches!(name.as_str(), "script" | "style" | "head") {
                    if closing {
                        skip_depth = skip_depth.saturating_sub(1);
                    } else if !self_closing {
                        skip_depth += 1;
                    }
                    continue;
                }
                if skip_depth > 0 {
                    continue;
                }
                match name.as_str() {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        if !closing {
                            ensure_blank_line(&mut out);
                            let level = name[1..].parse::<usize>().unwrap_or(1);
                            out.push_str(&"#".repeat(level));
                            out.push(' ');
                        } else {
                            out.push('\n');
                            out.push('\n');
                        }
                    }
                    "p" | "div" | "section" | "article" | "header" | "footer" | "table" => {
                        if !closing {
                            ensure_blank_line(&mut out);
                        } else {
                            out.push('\n');
                            out.push('\n');
                        }
                    }
                    "br" => out.push_str("  \n"),
                    "hr" => {
                        ensure_blank_line(&mut out);
                        out.push_str("---\n\n");
                    }
                    "strong" | "b" => out.push_str("**"),
                    "em" | "i" => out.push('*'),
                    "code" if pre_depth == 0 => out.push('`'),
                    "pre" => {
                        if !closing {
                            ensure_blank_line(&mut out);
                            out.push_str("```\n");
                            pre_depth += 1;
                        } else {
                            pre_depth = pre_depth.saturating_sub(1);
                            if !out.ends_with('\n') {
                                out.push('\n');
                            }
                            out.push_str("```\n\n");
                        }
                    }
                    "blockquote" => {
                        if !closing {
                            ensure_blank_line(&mut out);
                            out.push_str("> ");
                        } else {
                            out.push('\n');
                            out.push('\n');
                        }
                    }
                    "ul" => {
                        if !closing {
                            list_stack.push('-');
                        } else {
                            list_stack.pop();
                            ensure_blank_line(&mut out);
                        }
                    }
                    "ol" => {
                        if !closing {
                            list_stack.push('1');
                        } else {
                            list_stack.pop();
                            ensure_blank_line(&mut out);
                        }
                    }
                    "li" => {
                        if !closing {
                            if !out.is_empty() && !out.ends_with('\n') {
                                out.push('\n');
                            }
                            let indent = "  ".repeat(list_stack.len().saturating_sub(1));
                            let marker = match list_stack.last() {
                                Some('1') => "1. ",
                                _ => "- ",
                            };
                            out.push_str(&indent);
                            out.push_str(marker);
                        } else {
                            out.push('\n');
                        }
                    }
                    "a" => {
                        if !closing {
                            pending_href.push(attr(&attrs, "href").map(str::to_string));
                            out.push('[');
                        } else if let Some(href) = pending_href.pop().flatten() {
                            out.push(']');
                            out.push('(');
                            out.push_str(&href);
                            out.push(')');
                        } else {
                            out.push(']');
                            pending_href.pop();
                        }
                    }
                    "img" => {
                        let alt = attr(&attrs, "alt").unwrap_or("");
                        let src = attr(&attrs, "src").unwrap_or("");
                        out.push_str("![");
                        out.push_str(alt);
                        out.push_str("](");
                        out.push_str(src);
                        out.push(')');
                    }
                    _ => {}
                }
            }
        }
    }

    let trimmed = out.trim_end();
    let mut result = String::with_capacity(trimmed.len() + 1);
    result.push_str(trimmed);
    result.push('\n');
    result
}

/// Converts an HTML document to reader-friendly plain text.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut skip_depth = 0u32;

    for tok in tokenize(html) {
        match tok {
            Token::Text(t) => {
                if skip_depth > 0 {
                    continue;
                }
                let text = collapse_ws(&decode_entities(t));
                if text.trim().is_empty() {
                    continue;
                }
                if !out.is_empty() && !out.ends_with('\n') && !out.ends_with(' ') {
                    out.push(' ');
                }
                out.push_str(text.trim_start());
            }
            Token::Tag {
                name,
                closing,
                self_closing,
                ..
            } => {
                if matches!(name.as_str(), "script" | "style" | "head") {
                    if closing {
                        skip_depth = skip_depth.saturating_sub(1);
                    } else if !self_closing {
                        skip_depth += 1;
                    }
                    continue;
                }
                if skip_depth > 0 {
                    continue;
                }
                let is_block = matches!(
                    name.as_str(),
                    "p" | "div"
                        | "section"
                        | "article"
                        | "header"
                        | "footer"
                        | "li"
                        | "tr"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "br"
                        | "hr"
                        | "ul"
                        | "ol"
                        | "table"
                        | "blockquote"
                        | "pre"
                );
                if is_block && (closing || name == "br" || name == "hr") {
                    if !out.ends_with("\n\n") {
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                        out.push('\n');
                    }
                }
            }
        }
    }

    let trimmed = out.trim();
    format!("{trimmed}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_headings_and_paragraphs() {
        let md = html_to_markdown("<h1>Title</h1><p>Hello <b>world</b>.</p>");
        assert!(md.contains("# Title"));
        assert!(md.contains("Hello **world**."));
    }

    #[test]
    fn converts_links_and_images() {
        let md = html_to_markdown("<a href=\"https://example.com\">docs</a>");
        assert_eq!(md.trim(), "[docs](https://example.com)");
        let md = html_to_markdown("<img src=\"a.png\" alt=\"Alt\">");
        assert_eq!(md.trim(), "![Alt](a.png)");
    }

    #[test]
    fn converts_lists() {
        let md = html_to_markdown("<ul><li>one</li><li>two</li></ul>");
        assert!(md.contains("- one"));
        assert!(md.contains("- two"));
    }

    #[test]
    fn skips_script_and_style() {
        let md = html_to_markdown("<style>body{color:red}</style><p>Hi</p><script>alert(1)</script>");
        assert!(!md.contains("color:red"));
        assert!(!md.contains("alert"));
        assert!(md.contains("Hi"));
    }

    #[test]
    fn text_strips_tags_with_block_breaks() {
        let txt = html_to_text("<h1>Title</h1><p>One</p><p>Two</p>");
        assert!(txt.contains("Title"));
        assert!(txt.contains("One"));
        assert!(txt.contains("Two"));
        assert!(!txt.contains('<'));
    }
}

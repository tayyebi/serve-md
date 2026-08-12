use std::collections::HashMap;

fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// A tiny, dependency-free template engine.
///
/// Supported tags:
/// - `{{name}}`      — insert value (HTML-escaped)
/// - `{{raw:name}}`  — insert value verbatim (no escaping)
/// - `{{#if flag}}…{{/if}}`  — include block only when `flag` is set
/// - `{{#for rows}}…{{/for}}` — repeat block once per row
pub struct Template<'a> {
    content: &'a str,
}

impl<'a> Template<'a> {
    pub fn new(content: &'a str) -> Self {
        Self { content }
    }

    pub fn render(
        &self,
        vars: &HashMap<&str, &str>,
        rows: &[Vec<(String, String)>],
        flags: &[&str],
    ) -> String {
        let mut out = String::new();
        let content = self.content;
        let mut i = 0usize;
        while i < content.len() {
            let rest = &content[i..];
            let Some(pos) = rest.find("{{") else {
                out.push_str(rest);
                break;
            };
            out.push_str(&content[i..i + pos]);
            let tag_start = i + pos + 2;
            let Some(close_rel) = content[tag_start..].find("}}") else {
                out.push_str(&content[i + pos..]);
                break;
            };
            let raw_tag = &content[tag_start..tag_start + close_rel];
            let tag = raw_tag.trim();
            let lower = tag.to_ascii_lowercase();
            let body_start = tag_start + close_rel + 2;

            if lower.starts_with("#if ") {
                let name = tag["#if ".len()..].trim();
                let enabled = flags.contains(&name);
                match find_block(content, body_start, "#if ", "/if") {
                    Some((body, end)) => {
                        if enabled {
                            out.push_str(&Template::new(body).render(vars, rows, flags));
                        }
                        i = end;
                    }
                    None => {
                        out.push_str(&content[i..body_start]);
                        i = body_start;
                    }
                }
            } else if lower.starts_with("#for ") {
                match find_block(content, body_start, "#for ", "/for") {
                    Some((body, end)) => {
                        let sub = Template::new(body);
                        for row in rows {
                            let mut merged: HashMap<&str, &str> =
                                HashMap::with_capacity(vars.len() + row.len());
                            for (k, v) in vars {
                                merged.insert(k, v);
                            }
                            for (k, v) in row {
                                merged.insert(k.as_str(), v.as_str());
                            }
                            out.push_str(&sub.render(&merged, &[], flags));
                        }
                        i = end;
                    }
                    None => {
                        out.push_str(&content[i..body_start]);
                        i = body_start;
                    }
                }
            } else if lower.starts_with("/if") || lower.starts_with("/for") {
                out.push_str(&content[i..body_start]);
                i = body_start;
            } else {
                let (key, raw) = match tag.strip_prefix("raw:") {
                    Some(k) => (k, true),
                    None => (tag, false),
                };
                if let Some(v) = vars.get(key) {
                    if raw {
                        out.push_str(v);
                    } else {
                        out.push_str(&escape_html(v));
                    }
                }
                i = body_start;
            }
        }
        out
    }
}

fn find_block<'a>(
    content: &'a str,
    from: usize,
    open_kind: &str,
    close_kind: &str,
) -> Option<(&'a str, usize)> {
    let mut depth = 0usize;
    for (idx, _) in content[from..].match_indices("{{") {
        let pos = from + idx;
        let tag_start = pos + 2;
        let Some(close_rel) = content[tag_start..].find("}}") else {
            continue;
        };
        let tag = content[tag_start..tag_start + close_rel].trim();
        let lower = tag.to_ascii_lowercase();
        if lower.starts_with(open_kind) {
            depth += 1;
        } else if lower.starts_with(close_kind) {
            if depth == 0 {
                return Some((&content[from..pos], tag_start + close_rel + 2));
            }
            depth -= 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var<'a>(name: &'a str, value: &'a str) -> HashMap<&'a str, &'a str> {
        let mut m = HashMap::new();
        m.insert(name, value);
        m
    }

    #[test]
    fn substitutes_and_escapes() {
        let t = Template::new("Hello {{name}}!");
        assert_eq!(
            t.render(&var("name", "<b>World</b>"), &[], &[]),
            "Hello &lt;b&gt;World&lt;/b&gt;!"
        );
    }

    #[test]
    fn missing_var_is_empty() {
        let t = Template::new("[{{nope}}]");
        assert_eq!(t.render(&HashMap::new(), &[], &[]), "[]");
    }

    #[test]
    fn raw_passthrough() {
        let t = Template::new("<main>{{raw:body}}</main>");
        assert_eq!(
            t.render(&var("body", "<article>x & y</article>"), &[], &[]),
            "<main><article>x & y</article></main>"
        );
    }

    #[test]
    fn if_blocks() {
        let t = Template::new("A{{#if x}}B{{/if}}C");
        assert_eq!(t.render(&HashMap::new(), &[], &["x"]), "ABC");
        assert_eq!(t.render(&HashMap::new(), &[], &[]), "AC");
    }

    #[test]
    fn for_blocks() {
        let t = Template::new("{{#for item}}[{{n}}]{{/for}}");
        let rows = vec![
            vec![("n".to_string(), "1".to_string())],
            vec![("n".to_string(), "2".to_string())],
        ];
        assert_eq!(t.render(&HashMap::new(), &rows, &[]), "[1][2]");
    }

    #[test]
    fn for_merges_outer_vars() {
        let t = Template::new("{{#for item}}{{greet}} {{n}};{{/for}}");
        let rows = vec![vec![("n".to_string(), "x".to_string())]];
        assert_eq!(t.render(&var("greet", "hi"), &rows, &[]), "hi x;");
    }

    #[test]
    fn nested_if_in_for() {
        let t = Template::new("{{#for item}}{{#if b}}{{n}}{{/if}}{{/for}}");
        let rows = vec![vec![("n".to_string(), "1".to_string())]];
        assert_eq!(t.render(&HashMap::new(), &rows, &["b"]), "1");
        assert_eq!(t.render(&HashMap::new(), &rows, &[]), "");
    }

    #[test]
    fn literal_open_brace() {
        let t = Template::new("a {{ b c");
        assert_eq!(t.render(&HashMap::new(), &[], &[]), "a {{ b c");
    }
}

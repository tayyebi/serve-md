//! What a Markdown document says about itself: its title, a one-line summary,
//! and its heading outline.
//!
//! Three callers need this and must agree: the generated `llms.txt` names each
//! document by its title, the `list_docs` MCP tool reports the same title, and
//! `get_outline` and `search_docs` hand out heading anchors that have to match
//! the `id` attributes in the rendered HTML.
//!
//! That last point is why anchors come from [`comrak::Anchorizer`] rather than
//! a slug function written here. comrak generates heading `id`s with it when
//! `extension.header_id_prefix` is set — which the `webmcp` plugin turns on —
//! so using the same type is what guarantees `#getting-started` in an answer
//! actually resolves on the page. A reimplementation would drift, and the
//! symptom would be dead links in an agent's citations.

use comrak::nodes::{AstNode, NodeValue};
use comrak::{parse_document, Anchorizer, Arena, Options};

/// The longest summary [`summary`] will produce, in characters. Long enough
/// for a useful sentence, short enough that a generated `llms.txt` stays
/// scannable.
const MAX_SUMMARY: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    /// 1-6.
    pub level: u8,
    pub text: String,
    /// The `id` comrak gives this heading, so `#anchor` links resolve.
    pub anchor: String,
    /// 1-based source line, for matching a search hit to the section it is in.
    pub line: u64,
}

/// Every heading in the document, in source order.
pub fn headings(src: &str, options: &Options<'_>) -> Vec<Heading> {
    let arena = Arena::new();
    let root = parse_document(&arena, src, options);
    // One anchorizer per document, exactly as comrak uses one per output file:
    // it is what appends `-1` to the second "Overview" so the two headings do
    // not share an id.
    let mut anchorizer = Anchorizer::new();
    let mut out = Vec::new();
    for node in root.descendants() {
        let (level, line) = {
            let data = node.data.borrow();
            match data.value {
                NodeValue::Heading(h) => (h.level, data.sourcepos.start.line as u64),
                _ => continue,
            }
        };
        let text = inline_text(node);
        let anchor = anchorizer.anchorize(&text);
        out.push(Heading { level, text, anchor, line });
    }
    out
}

/// What a document says about itself in one pass: title, prose word count, and
/// how many headings it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brief {
    /// The first level-1 heading, or failing that the first heading of any
    /// level. `None` for a document with no headings at all.
    pub title: Option<String>,
    /// Words of prose. Fenced code is excluded — a document is not longer to
    /// read because it quotes a config file.
    pub words: usize,
    pub headings: usize,
}

/// Everything in [`Brief`], from a single parse.
///
/// One pass rather than three because the `x-headers` plugin wants all of it on
/// every document request, and the response is already paying for the render's
/// own parse. Unlike [`headings`] this allocates no [`Anchorizer`]: nothing
/// here hands out `#anchor` links.
pub fn brief(src: &str, options: &Options<'_>) -> Brief {
    let arena = Arena::new();
    let root = parse_document(&arena, src, options);
    let mut first_h1: Option<String> = None;
    let mut first_any: Option<String> = None;
    let mut count = 0usize;
    let mut words = 0usize;
    for node in root.descendants() {
        // `Text` and `Code` beneath a heading are visited here too, so heading
        // words count once, as words — which is what a reader would say.
        let level = {
            let data = node.data.borrow();
            match &data.value {
                NodeValue::Heading(h) => Some(h.level),
                NodeValue::Text(t) => {
                    words += t.split_whitespace().count();
                    None
                }
                NodeValue::Code(c) => {
                    words += c.literal.split_whitespace().count();
                    None
                }
                _ => None,
            }
        };
        let Some(level) = level else { continue };
        count += 1;
        let text = inline_text(node);
        if level == 1 && first_h1.is_none() {
            first_h1 = Some(text.clone());
        }
        if first_any.is_none() {
            first_any = Some(text);
        }
    }
    Brief {
        title: first_h1.or(first_any),
        words,
        headings: count,
    }
}

/// The document's title: its first level-1 heading, or failing that its first
/// heading of any level. `None` for a document with no headings at all, which
/// leaves the caller to fall back to the filename.
pub fn title(src: &str, options: &Options<'_>) -> Option<String> {
    brief(src, options).title
}

/// A one-line summary: the first sentence of the first paragraph.
///
/// Whitespace is collapsed so the result is safe to place on a single line of
/// a generated `llms.txt`, where a newline would break the list item.
pub fn summary(src: &str, options: &Options<'_>) -> Option<String> {
    let arena = Arena::new();
    let root = parse_document(&arena, src, options);
    let para = root
        .descendants()
        .find(|n| matches!(n.data.borrow().value, NodeValue::Paragraph))?;
    let text = collapse_ws(&inline_text(para));
    if text.is_empty() {
        return None;
    }
    Some(first_sentence(&text))
}

/// The heading a given source line falls under — the last one at or above it.
pub fn heading_at(headings: &[Heading], line: u64) -> Option<&Heading> {
    headings.iter().rev().find(|h| h.line <= line)
}

/// The visible text of a node: every `Text` and `Code` literal beneath it,
/// with emphasis and link markup dropped.
fn inline_text<'a>(node: &'a AstNode<'a>) -> String {
    let mut out = String::new();
    for child in node.descendants() {
        match &child.data.borrow().value {
            NodeValue::Text(t) => out.push_str(t),
            NodeValue::Code(c) => out.push_str(&c.literal),
            NodeValue::LineBreak | NodeValue::SoftBreak => out.push(' '),
            _ => {}
        }
    }
    out.trim().to_string()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cuts at the first sentence end, or at [`MAX_SUMMARY`] characters on a
/// word boundary if no sentence ends before then.
///
/// A period only ends a sentence when a space follows, so `serve-md v0.4.0`
/// and `file.md` stay intact.
fn first_sentence(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut end = None;
    for (i, c) in text.char_indices() {
        if matches!(c, '.' | '!' | '?') && bytes.get(i + 1).is_some_and(|b| *b == b' ') {
            end = Some(i + 1);
            break;
        }
        if i >= MAX_SUMMARY {
            break;
        }
    }
    match end {
        Some(i) => text[..i].to_string(),
        None if text.chars().count() <= MAX_SUMMARY => text.to_string(),
        None => {
            let cut: String = text.chars().take(MAX_SUMMARY).collect();
            let trimmed = match cut.rfind(' ') {
                Some(i) => &cut[..i],
                None => &cut[..],
            };
            format!("{}…", trimmed.trim_end_matches([',', ';', ':']))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Set;

    fn opts() -> Options<'static> {
        Set::default().options()
    }

    #[test]
    fn brief_reports_title_words_and_heading_count() {
        let src = "# One\n\nsome plain words here\n\n## Two\n\nmore\n";
        let b = brief(src, &opts());
        assert_eq!(b.title.as_deref(), Some("One"));
        assert_eq!(b.headings, 2);
        // "One" + "some plain words here" + "Two" + "more"
        assert_eq!(b.words, 7);
    }

    #[test]
    fn brief_does_not_count_fenced_code_as_prose() {
        let plain = brief("two words\n", &opts()).words;
        let with_code = brief("two words\n\n```\nlet x = 1 + 2 + 3;\n```\n", &opts()).words;
        assert_eq!(plain, with_code);
        // Inline code is prose, though: it reads as part of the sentence.
        assert_eq!(brief("a `b` c\n", &opts()).words, 3);
    }

    #[test]
    fn brief_falls_back_past_a_missing_h1() {
        assert_eq!(brief("## Only\n", &opts()).title.as_deref(), Some("Only"));
        let b = brief("no headings at all\n", &opts());
        assert_eq!(b.title, None);
        assert_eq!(b.headings, 0);
    }

    #[test]
    fn headings_are_listed_in_order_with_levels() {
        let src = "# One\n\ntext\n\n## Two\n\n### Three\n";
        let hs = headings(src, &opts());
        assert_eq!(
            hs.iter().map(|h| (h.level, h.text.as_str())).collect::<Vec<_>>(),
            vec![(1, "One"), (2, "Two"), (3, "Three")]
        );
        assert_eq!(hs[0].line, 1);
        assert_eq!(hs[1].line, 5);
    }

    #[test]
    fn anchors_match_comraks_own_ids() {
        // The whole point: these strings end up in agent-visible links.
        let src = "# Getting Started\n\n## Isn't it grand?\n";
        let hs = headings(src, &opts());
        assert_eq!(hs[0].anchor, "getting-started");
        assert_eq!(hs[1].anchor, "isnt-it-grand");
    }

    #[test]
    fn repeated_headings_get_unique_anchors() {
        let hs = headings("# Overview\n\n## Overview\n", &opts());
        assert_eq!(hs[0].anchor, "overview");
        assert_eq!(hs[1].anchor, "overview-1");
    }

    #[test]
    fn heading_text_drops_markup_but_keeps_code() {
        let hs = headings("# The `--dir` *flag*\n", &opts());
        assert_eq!(hs[0].text, "The --dir flag");
        assert_eq!(hs[0].anchor, "the---dir-flag");
    }

    #[test]
    fn title_prefers_h1_then_falls_back() {
        assert_eq!(title("## Sub\n\n# Main\n", &opts()).as_deref(), Some("Main"));
        assert_eq!(title("## Only\n", &opts()).as_deref(), Some("Only"));
        assert_eq!(title("just prose\n", &opts()), None);
    }

    #[test]
    fn summary_is_the_first_sentence_of_the_first_paragraph() {
        let src = "# Title\n\nFirst sentence here. Second one follows.\n\nA later paragraph.\n";
        assert_eq!(
            summary(src, &opts()).as_deref(),
            Some("First sentence here.")
        );
    }

    #[test]
    fn summary_collapses_newlines_so_it_fits_one_line() {
        let src = "Wrapped across\ntwo source lines and more\n";
        let s = summary(src, &opts()).unwrap();
        assert!(!s.contains('\n'));
        assert_eq!(s, "Wrapped across two source lines and more");
    }

    #[test]
    fn a_period_inside_a_token_does_not_end_the_sentence() {
        let src = "Run serve-md v0.4.0 on ./docs now. Then stop.\n";
        assert_eq!(
            summary(src, &opts()).as_deref(),
            Some("Run serve-md v0.4.0 on ./docs now.")
        );
    }

    #[test]
    fn a_long_unpunctuated_paragraph_is_cut_on_a_word_boundary() {
        let src = "word ".repeat(200);
        let s = summary(&src, &opts()).unwrap();
        assert!(s.chars().count() <= MAX_SUMMARY + 1, "got {} chars", s.chars().count());
        assert!(s.ends_with('…'));
        assert!(!s.contains("wor…"), "cut mid-word: {s}");
    }

    #[test]
    fn documents_without_prose_have_no_summary() {
        assert_eq!(summary("", &opts()), None);
        assert_eq!(summary("# Only a heading\n", &opts()), None);
    }

    #[test]
    fn heading_at_finds_the_enclosing_section() {
        let src = "# One\n\na\n\n## Two\n\nb\n\n## Three\n\nc\n";
        let hs = headings(src, &opts());
        assert_eq!(heading_at(&hs, 3).unwrap().text, "One");
        assert_eq!(heading_at(&hs, 7).unwrap().text, "Two");
        assert_eq!(heading_at(&hs, 99).unwrap().text, "Three");
        // A line before any heading has no section.
        let hs2 = headings("intro\n\n# Later\n", &opts());
        assert!(heading_at(&hs2, 1).is_none());
    }
}

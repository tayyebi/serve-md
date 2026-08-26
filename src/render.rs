//! Format conversion for each supported source `FileKind`, so callers (the
//! HTTP routes) negotiate content type without knowing per-format details.

use crate::ascii::markdown_to_ascii;
use crate::convert::{html_to_markdown, html_to_text};
use crate::plugin::{Rendered, Set};
use crate::scanner::FileKind;

/// Renders `src` to an HTML fragment/document suitable for a browser, along
/// with any `<head>` markup the active plugins asked for.
///
/// Only meaningful for `Markdown`/`Html`; `Static` files are served as raw
/// bytes by the caller and never reach this function.
pub fn to_html(kind: FileKind, src: &str, plugins: &Set) -> Rendered {
    match kind {
        FileKind::Markdown => plugins.render_html(src),
        FileKind::Html | FileKind::Static => Rendered {
            html: src.to_string(),
            head: String::new(),
        },
    }
}

/// Renders `src` as Markdown source.
pub fn to_markdown(kind: FileKind, src: &str) -> String {
    match kind {
        FileKind::Markdown | FileKind::Static => src.to_string(),
        FileKind::Html => html_to_markdown(src),
    }
}

/// Renders `src` as reader-friendly plain text (terminal clients).
pub fn to_text(kind: FileKind, src: &str, plugins: &Set) -> String {
    match kind {
        FileKind::Markdown => markdown_to_ascii(src, &plugins.options()),
        FileKind::Html => html_to_text(src),
        FileKind::Static => src.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn math() -> Set {
        Set::resolve(&["math".to_string()]).unwrap()
    }

    #[test]
    fn markdown_passthrough_and_conversion() {
        let none = Set::default();
        assert_eq!(to_markdown(FileKind::Markdown, "# Hi"), "# Hi");
        assert!(to_html(FileKind::Markdown, "# Hi", &none)
            .html
            .contains("<h1>Hi</h1>"));
        assert!(to_text(FileKind::Markdown, "# Hi", &none).contains("HI"));
    }

    #[test]
    fn html_conversion() {
        let none = Set::default();
        let src = "<h1>Hi</h1>";
        assert_eq!(to_html(FileKind::Html, src, &none).html, src);
        assert!(to_markdown(FileKind::Html, src).contains("# Hi"));
        assert!(to_text(FileKind::Html, src, &none).contains("Hi"));
    }

    #[test]
    fn head_is_populated_only_with_a_plugin_active() {
        let src = "$E = mc^2$\n";
        assert!(to_html(FileKind::Markdown, src, &Set::default())
            .head
            .is_empty());
        assert!(!to_html(FileKind::Markdown, src, &math()).head.is_empty());
    }

    #[test]
    fn html_files_never_gain_head_markup() {
        let out = to_html(FileKind::Html, "<p>$E = mc^2$</p>", &math());
        assert!(out.head.is_empty());
    }

    #[test]
    fn markdown_source_keeps_math_verbatim() {
        assert_eq!(to_markdown(FileKind::Markdown, "$E = mc^2$"), "$E = mc^2$");
    }

    /// Renders Markdown to HTML with no plugins — the default pipeline.
    fn html(src: &str) -> String {
        to_html(FileKind::Markdown, src, &Set::default()).html
    }

    /// Renders Markdown to plain text with no plugins.
    fn text(src: &str) -> String {
        to_text(FileKind::Markdown, src, &Set::default())
    }

    #[test]
    fn html_renders_block_structure() {
        let out = html("# Title\n\n## Sub\n\nA paragraph.\n\n> quoted\n\n---\n");
        assert!(out.contains("<h1>Title</h1>"), "{out}");
        assert!(out.contains("<h2>Sub</h2>"), "{out}");
        assert!(out.contains("<p>A paragraph.</p>"), "{out}");
        assert!(out.contains("<blockquote>"), "{out}");
        assert!(out.contains("<hr />"), "{out}");
    }

    #[test]
    fn html_renders_inline_emphasis_and_code() {
        let out = html("Hello **bold**, *em*, `code`.\n");
        assert!(out.contains("<strong>bold</strong>"), "{out}");
        assert!(out.contains("<em>em</em>"), "{out}");
        assert!(out.contains("<code>code</code>"), "{out}");
    }

    #[test]
    fn html_renders_lists() {
        let ul = html("- one\n- two\n");
        assert!(ul.contains("<ul>"), "{ul}");
        assert!(ul.contains("<li>one</li>"), "{ul}");
        let ol = html("1. first\n2. second\n");
        assert!(ol.contains("<ol>"), "{ol}");
        assert!(ol.contains("<li>first</li>"), "{ol}");
    }

    #[test]
    fn html_renders_links_and_images() {
        let out = html("[docs](https://example.com) and ![Alt](a.png)\n");
        assert!(out.contains("<a href=\"https://example.com\">docs</a>"), "{out}");
        assert!(out.contains("src=\"a.png\""), "{out}");
        assert!(out.contains("alt=\"Alt\""), "{out}");
    }

    #[test]
    fn html_renders_fenced_code_with_language_class() {
        let out = html("```rust\nfn main() {}\n```\n");
        assert!(out.contains("<pre"), "{out}");
        assert!(out.contains("<code"), "{out}");
        assert!(out.contains("language-rust"), "{out}");
        // Code content is escaped, never emitted as live markup.
        assert!(out.contains("fn main() {}"), "{out}");
    }

    #[test]
    fn html_renders_enabled_gfm_extensions() {
        assert!(html("~~gone~~\n").contains("<del>gone</del>"));
        assert!(html("| A | B |\n|---|---|\n| 1 | 2 |\n").contains("<table>"));
        assert!(html("- [ ] todo\n").contains("type=\"checkbox\""));
        assert!(html("see https://example.com now\n").contains("<a href=\"https://example.com\">"));
    }

    #[test]
    fn html_escapes_text_and_suppresses_raw_markup() {
        let out = html("5 < 6 & 7 > 4\n");
        assert!(out.contains("&lt;"), "{out}");
        assert!(out.contains("&amp;"), "{out}");

        // Raw HTML is not passed through: comrak runs with unsafe_ off, so a
        // Markdown file can never inject markup into the served page.
        let raw = html("<div onclick=\"x()\">hi</div>\n");
        assert!(!raw.contains("<div"), "{raw}");
        assert!(!raw.contains("onclick"), "{raw}");
    }

    #[test]
    fn html_filters_dangerous_inline_tags() {
        let out = html("text <script>alert(1)</script> more\n");
        assert!(!out.contains("<script>"), "{out}");
    }

    #[test]
    fn empty_markdown_renders_empty_html() {
        assert_eq!(html(""), "");
    }

    #[test]
    fn text_renders_headings_lists_and_tables() {
        let out = text("# Title\n\n- one\n- two\n\n| A | B |\n|---|---|\n| 1 | 2 |\n");
        assert!(out.contains("TITLE"), "{out}");
        assert!(out.contains("- one"), "{out}");
        assert!(out.contains("| A"), "{out}");
        // No markup ever reaches a terminal client.
        assert!(!out.contains('<'), "{out}");
    }

    fn mermaid() -> Set {
        Set::resolve(&["mermaid".to_string()]).unwrap()
    }

    fn both() -> Set {
        Set::resolve(&["math".to_string(), "mermaid".to_string()]).unwrap()
    }

    #[test]
    fn math_plugin_typesets_markdown_to_html() {
        let out = to_html(FileKind::Markdown, "energy $E = mc^2$ today\n", &math());
        assert!(out.html.contains("<math"), "{}", out.html);
        assert!(out.head.contains("<style>"), "{}", out.head);
    }

    #[test]
    fn mermaid_plugin_draws_markdown_to_html() {
        let src = "```mermaid\nflowchart TD\n  A[Start] --> B[End]\n```\n";
        let out = to_html(FileKind::Markdown, src, &mermaid());
        assert!(out.html.contains("<svg"), "{}", out.html);
        assert!(!out.html.contains("language-mermaid"), "{}", out.html);
        assert!(out.head.contains("<style>"), "{}", out.head);
    }

    #[test]
    fn both_plugins_render_in_one_document() {
        let src = "$E = mc^2$\n\n```mermaid\nflowchart LR\n  A --> B\n```\n";
        let out = to_html(FileKind::Markdown, src, &both());
        assert!(out.html.contains("<math"), "{}", out.html);
        assert!(out.html.contains("<svg"), "{}", out.html);
        // Each plugin contributes its own head markup.
        assert!(out.head.contains("math{"), "{}", out.head);
        assert!(out.head.contains(".mmd"), "{}", out.head);
    }

    #[test]
    fn a_plugin_contributes_no_head_markup_to_a_document_it_did_not_touch() {
        // Both active, only math fires.
        let out = to_html(FileKind::Markdown, "$E = mc^2$\n", &both());
        assert!(out.head.contains("math{"), "{}", out.head);
        assert!(!out.head.contains(".mmd"), "{}", out.head);
    }

    #[test]
    fn html_files_never_gain_mermaid_markup() {
        let src = "<pre><code class=\"language-mermaid\">flowchart TD\n A --> B</code></pre>";
        let out = to_html(FileKind::Html, src, &mermaid());
        assert_eq!(out.html, src);
        assert!(out.head.is_empty());
    }

    #[test]
    fn plugin_html_never_reaches_the_text_renderer() {
        // The terminal sees the original source constructs, not plugin output:
        // MathML and SVG are meaningless in a terminal.
        let math_txt = to_text(FileKind::Markdown, "energy $E = mc^2$ today\n", &math());
        assert!(math_txt.contains("$E = mc^2$"), "{math_txt}");
        assert!(!math_txt.contains("<math"), "{math_txt}");

        let src = "```mermaid\nflowchart TD\n  A[Start] --> B[End]\n```\n";
        let mermaid_txt = to_text(FileKind::Markdown, src, &mermaid());
        assert!(mermaid_txt.contains("    flowchart TD"), "{mermaid_txt}");
        assert!(!mermaid_txt.contains("<svg"), "{mermaid_txt}");
    }

    #[test]
    fn markdown_source_is_verbatim_whatever_the_plugins() {
        let src = "$E = mc^2$\n\n```mermaid\nflowchart TD\n  A --> B\n```\n";
        assert_eq!(to_markdown(FileKind::Markdown, src), src);
    }

    // ---------------------------------------------------- markdown edge cases

    #[test]
    fn html_setext_headings_match_atx() {
        assert_eq!(html("Title\n=====\n"), html("# Title\n"));
        assert_eq!(html("Sub\n---\n"), html("## Sub\n"));
    }

    #[test]
    fn html_heading_levels_go_to_six() {
        assert!(html("###### Deep\n").contains("<h6>Deep</h6>"));
        // Seven hashes is not a heading.
        assert!(!html("####### Deeper\n").contains("<h7"));
    }

    #[test]
    fn html_hard_break_becomes_a_br() {
        assert!(html("one  \ntwo\n").contains("<br />"), "two-space break");
        assert!(html("one\\\ntwo\n").contains("<br />"), "backslash break");
        // A soft break is just a newline, not a <br>.
        assert!(!html("one\ntwo\n").contains("<br"));
    }

    #[test]
    fn html_nested_and_loose_lists() {
        let nested = html("- one\n  - inner\n");
        assert_eq!(nested.matches("<ul>").count(), 2, "{nested}");
        // A loose list wraps each item's text in a paragraph; a tight one does not.
        assert!(html("- a\n\n- b\n").contains("<p>a</p>"));
        assert!(!html("- a\n- b\n").contains("<p>a</p>"));
    }

    #[test]
    fn html_blockquote_can_contain_blocks() {
        let out = html("> ## Quoted\n>\n> - item\n");
        assert!(out.contains("<blockquote>"), "{out}");
        assert!(out.contains("<h2>Quoted</h2>"), "{out}");
        assert!(out.contains("<li>item</li>"), "{out}");
    }

    #[test]
    fn html_reference_links_and_titles_resolve() {
        let out = html("[docs][d]\n\n[d]: https://example.com \"Docs\"\n");
        assert!(out.contains("href=\"https://example.com\""), "{out}");
        assert!(out.contains("title=\"Docs\""), "{out}");
        // The definition itself is not rendered.
        assert!(!out.contains("[d]:"), "{out}");
    }

    #[test]
    fn html_indented_code_blocks_are_preserved() {
        let out = html("    let x = 1;\n");
        assert!(out.contains("<pre><code>"), "{out}");
        assert!(out.contains("let x = 1;"), "{out}");
        assert!(!out.contains("language-"), "{out}");
    }

    #[test]
    fn html_code_spans_keep_their_content_literal() {
        let out = html("use `a < b && c` here\n");
        assert!(out.contains("<code>a &lt; b &amp;&amp; c</code>"), "{out}");
        // Double backticks let a code span contain a backtick.
        assert!(html("``a ` b``\n").contains("<code>a ` b</code>"));
    }

    #[test]
    fn html_backslash_escapes_suppress_markup() {
        let out = html("\\*not emphasis\\* and \\# not a heading\n");
        assert!(!out.contains("<em>"), "{out}");
        assert!(!out.contains("<h1>"), "{out}");
        assert!(out.contains("*not emphasis*"), "{out}");
    }

    #[test]
    fn html_named_entities_decode() {
        let out = html("caf&eacute; &copy; 2026\n");
        assert!(out.contains('\u{e9}'), "{out}");
        assert!(out.contains('\u{a9}'), "{out}");
    }

    #[test]
    fn html_nested_emphasis() {
        let out = html("***both*** and **bold with *em* inside**\n");
        assert!(out.contains("<em>") && out.contains("<strong>"), "{out}");
    }

    #[test]
    fn html_tables_get_a_head_and_body() {
        let out = html("| A | B |\n|---|--:|\n| 1 | 2 |\n| 3 | 4 |\n");
        assert!(out.contains("<thead>"), "{out}");
        assert!(out.contains("<th"), "{out}");
        assert!(out.contains("<tbody>"), "{out}");
        assert_eq!(out.matches("<tr>").count(), 3, "{out}");
    }

    #[test]
    fn html_task_list_records_checked_state() {
        let out = html("- [ ] todo\n- [x] done\n");
        assert_eq!(out.matches("type=\"checkbox\"").count(), 2, "{out}");
        assert_eq!(out.matches("checked").count(), 1, "{out}");
    }

    #[test]
    fn html_unsafe_link_schemes_are_neutralised() {
        // cmark's safe mode blanks dangerous hrefs; whatever the exact form,
        // the scheme must not survive as a live link.
        let out = html("[click](javascript:alert(1))\n");
        assert!(!out.contains("javascript:"), "{out}");
        let img = html("![x](data:text/html;base64,PHNjcmlwdD4=)\n");
        assert!(!img.contains("text/html"), "{img}");
    }

    #[test]
    fn html_disabled_extensions_stay_literal_text() {
        // Footnotes and header anchors are not enabled.
        let notes = html("text[^1]\n\n[^1]: a note\n");
        assert!(notes.contains("[^1]"), "{notes}");
        assert!(!notes.contains("footnote"), "{notes}");
        assert!(!html("# Title\n").contains("id=\"title\""));
        // Smart punctuation is off: dashes and quotes stay as typed.
        let punct = html("a -- b \"quoted\"\n");
        assert!(punct.contains("--"), "{punct}");
        assert!(!punct.contains('\u{201c}'), "{punct}");
    }

    #[test]
    fn html_handles_degenerate_input() {
        assert_eq!(html("   \n\n  \n"), "");
        assert_eq!(html("\n\n\n"), "");
        // An unterminated fence still closes at end of input.
        assert!(html("```\nunclosed\n").contains("</code></pre>"));
        // A table with no body rows is still a table.
        assert!(html("| A |\n|---|\n").contains("<table>"));
    }

    #[test]
    fn html_preserves_non_ascii_text() {
        let out = html("caf\u{e9} \u{2014} \u{6f22}\u{5b57} \u{1f600}\n");
        assert!(out.contains("caf\u{e9}"), "{out}");
        assert!(out.contains("\u{6f22}\u{5b57}"), "{out}");
        assert!(out.contains("\u{1f600}"), "{out}");
    }

    #[test]
    fn text_output_always_ends_with_one_newline() {
        for src in ["# Title", "para\n", "", "- a\n- b\n"] {
            let out = text(src);
            assert!(out.ends_with('\n'), "{out:?}");
            assert!(!out.ends_with("\n\n"), "{out:?}");
        }
    }
}

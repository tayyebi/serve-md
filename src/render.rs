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
}

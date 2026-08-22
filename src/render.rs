//! Format conversion for each supported source `FileKind`, so callers (the
//! HTTP routes) negotiate content type without knowing per-format details.

use crate::ascii::markdown_to_ascii;
use crate::convert::{html_to_markdown, html_to_text};
use crate::page::render_markdown;
use crate::scanner::FileKind;

/// Renders `src` to an HTML fragment/document suitable for a browser.
///
/// Only meaningful for `Markdown`/`Html`; `Static` files are served as raw
/// bytes by the caller and never reach this function.
pub fn to_html(kind: FileKind, src: &str) -> String {
    match kind {
        FileKind::Markdown => render_markdown(src),
        FileKind::Html | FileKind::Static => src.to_string(),
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
pub fn to_text(kind: FileKind, src: &str) -> String {
    match kind {
        FileKind::Markdown => markdown_to_ascii(src),
        FileKind::Html => html_to_text(src),
        FileKind::Static => src.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_passthrough_and_conversion() {
        assert_eq!(to_markdown(FileKind::Markdown, "# Hi"), "# Hi");
        assert!(to_html(FileKind::Markdown, "# Hi").contains("<h1>Hi</h1>"));
        assert!(to_text(FileKind::Markdown, "# Hi").contains("HI"));
    }

    #[test]
    fn html_conversion() {
        let src = "<h1>Hi</h1>";
        assert_eq!(to_html(FileKind::Html, src), src);
        assert!(to_markdown(FileKind::Html, src).contains("# Hi"));
        assert!(to_text(FileKind::Html, src).contains("Hi"));
    }
}

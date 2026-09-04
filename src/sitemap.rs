//! `/sitemap.xml`, generated from the served tree.
//!
//! Same shape as `llms.rs`: the catalog already knows every document this
//! server serves, so the sitemap is derived rather than a committed artefact
//! that falls out of date. Gated by `--plugin sitemap` (or its `--sitemap`
//! shorthand), and only ever answered when the served tree holds no file of
//! the same name — `http::route` checks that before this module runs.
//!
//! # Standard
//!
//! <https://www.sitemaps.org/protocol.html> — a `urlset` of `url` entries,
//! each a `<loc>` (required) and, here, a `<lastmod>` taken from the file's
//! mtime. `<changefreq>` and `<priority>` are the spec's own words "hints,
//! not directives" that crawlers are documented to ignore, so nothing here
//! bothers emitting them.
//!
//! # Why `<loc>` needs a caller-supplied origin
//!
//! A `<loc>` must be an absolute URL, and on the same site as the sitemap
//! itself. This server has no configured public hostname — see
//! `http::base_url`, which builds one from the request's own `Host` header
//! (and `X-Forwarded-Proto`, for the common case of a TLS-terminating proxy
//! in front of a plain-HTTP serve-md) — the same information a browser
//! actually reached this server through.

use crate::catalog::Snapshot;
use crate::encoding::percent_encode_path;
use crate::page::format_date;
use crate::scanner::FileKind;
use std::path::Path;

/// The documents worth listing: things with prose in them, the same test
/// `llms.rs` uses. A sitemap entry for a stylesheet or a favicon helps no
/// crawler.
fn is_document(rel: &str) -> bool {
    matches!(
        FileKind::from_path(Path::new(rel)),
        FileKind::Markdown | FileKind::Html
    )
}

/// Builds `/sitemap.xml`. `origin` is a scheme and host with no trailing
/// slash, e.g. `https://example.com`.
pub fn sitemap_xml(snap: &Snapshot, origin: &str) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    for f in &snap.files {
        if !is_document(&f.rel) {
            continue;
        }
        let loc = format!("{origin}{}", percent_encode_path(&format!("/{}", f.rel)));
        out.push_str("  <url>\n");
        out.push_str(&format!("    <loc>{}</loc>\n", xml_escape(&loc)));
        out.push_str(&format!("    <lastmod>{}</lastmod>\n", format_date(f.modified)));
        out.push_str("  </url>\n");
    }
    out.push_str("</urlset>\n");
    out
}

/// Escapes the five characters XML 1.0 requires escaped in text content and
/// attribute values. `origin` is caller-supplied (from a request header), so
/// this is what keeps a crafted `Host` from breaking out of `<loc>` rather
/// than merely producing an ugly URL.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "serve-md-sitemap-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            tag
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn snap(d: &Path) -> std::sync::Arc<Snapshot> {
        Catalog::scan(d).unwrap().current()
    }

    #[test]
    fn lists_every_document_as_an_absolute_url() {
        let d = tmp("basic");
        fs::write(d.join("a.md"), "# A\n").unwrap();
        fs::create_dir_all(d.join("guides")).unwrap();
        fs::write(d.join("guides/start.md"), "# Start\n").unwrap();
        let out = sitemap_xml(&snap(&d), "http://localhost:8080");
        assert!(out.contains("<loc>http://localhost:8080/a.md</loc>"), "{out}");
        assert!(
            out.contains("<loc>http://localhost:8080/guides/start.md</loc>"),
            "{out}"
        );
        assert!(out.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(out.contains("<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">"));
    }

    #[test]
    fn static_assets_are_not_listed() {
        let d = tmp("assets");
        fs::write(d.join("a.md"), "# A\n").unwrap();
        fs::write(d.join("logo.png"), "notreallyapng").unwrap();
        let out = sitemap_xml(&snap(&d), "http://localhost");
        assert!(!out.contains("logo.png"), "{out}");
    }

    #[test]
    fn each_entry_carries_a_lastmod_date() {
        let d = tmp("lastmod");
        fs::write(d.join("a.md"), "# A\n").unwrap();
        let out = sitemap_xml(&snap(&d), "http://localhost");
        assert!(out.contains("<lastmod>"), "{out}");
        // Four digits, a dash, two digits, a dash, two digits - never a
        // clock reading, which is not what the sitemap protocol wants here.
        let lastmod = out.split("<lastmod>").nth(1).unwrap().split('<').next().unwrap();
        assert_eq!(lastmod.len(), 10, "{lastmod}");
    }

    #[test]
    fn paths_with_spaces_are_percent_encoded() {
        let d = tmp("spaces");
        fs::write(d.join("my file.md"), "# Hi\n").unwrap();
        let out = sitemap_xml(&snap(&d), "http://localhost");
        assert!(out.contains("<loc>http://localhost/my%20file.md</loc>"), "{out}");
    }

    #[test]
    fn an_origin_that_could_break_the_xml_is_escaped() {
        let d = tmp("escape");
        fs::write(d.join("a.md"), "# A\n").unwrap();
        let out = sitemap_xml(&snap(&d), "http://\"><evil host");
        assert!(!out.contains("\"><evil"), "{out}");
        assert!(out.contains("&quot;&gt;&lt;evil"), "{out}");
    }

    #[test]
    fn an_empty_tree_is_still_a_valid_empty_urlset() {
        let out = sitemap_xml(&snap(&tmp("empty")), "http://localhost");
        assert!(out.contains("<urlset"));
        assert!(!out.contains("<url>"));
    }
}

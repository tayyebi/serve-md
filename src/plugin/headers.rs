//! The `x-headers` plugin: response headers that introduce the server and
//! describe the document being served.
//!
//! Without it serve-md answers with four headers and nothing else, which
//! leaves three gaps. A cache in front of the server cannot tell that
//! `serve_file` negotiated on `Accept` *and* `User-Agent`, so it may hand a
//! browser the plain-text representation. No client can revalidate anything,
//! because no validator is ever sent. And an agent learns nothing from a
//! `HEAD` request — not the title, not the size of the job, not that `/mcp`
//! exists.
//!
//! # Why the names carry no `X-` prefix
//!
//! RFC 6648 (June 2012) deprecated the `X-` convention for exactly the reason
//! it looks tidy: a prefixed header that turns out to be useful gets adopted
//! anyway, and then both spellings have to be supported forever. So standard
//! headers are used wherever one exists — `Server`, `Last-Modified`, `ETag`,
//! `Vary`, `Link` — and the rest are unprefixed `Doc-*`. Only the *flag* keeps
//! the familiar name, `--x-headers`.
//!
//! # Why this plugin adds no trait method
//!
//! [`Plugin`]'s three hooks are render-pipeline hooks: parser options, an AST
//! pass, and `<head>` markup. A fourth `headers()` hook would exist to serve
//! one plugin and would put a no-op default on `math` and `mermaid` forever.
//! Instead the router asks `plugins.has("x-headers")` and calls the free
//! functions below — the same shape `webmcp` uses to gate `/mcp` and
//! `/llms.txt`, and what [`crate::plugin::Set::has`] is for.
//!
//! # What this discloses
//!
//! Modification time, size, title and the word and heading counts are all
//! already visible: the listing page prints size and mtime for every file, and
//! the rest is in the body being served. The one genuinely new disclosure is
//! the version string in `Server`, which helps fingerprint a public deploy.
//! That is why this is opt-in rather than the default.
//!
//! # References
//!
//! - RFC 9110 §5.5 (field values are ASCII), §5.6.7 (IMF-fixdate),
//!   §8.8.3 (`ETag`), §12.5.5 (`Vary`), §13.1.1-3 (conditional requests)
//! - RFC 8288 §3 (`Link`)
//! - RFC 8187 §3.2 (`ext-value`, the `filename*` convention)
//! - RFC 6648 (deprecating `X-`)

use super::Plugin;
use crate::docmeta::Brief;
use crate::encoding::{percent_encode_attr, percent_encode_path};
use crate::page;
use crate::scanner::FileKind;
use std::fs::Metadata;
use std::time::UNIX_EPOCH;

pub struct XHeaders;

impl Plugin for XHeaders {
    fn name(&self) -> &'static str {
        "x-headers"
    }

    fn describe(&self) -> &'static str {
        "describe the server and each document in response headers (Last-Modified, ETag, Vary, Link, Doc-*)"
    }

    // configure/transform/head keep their defaults: this plugin changes no
    // markup at all, only what goes above the blank line.
}

/// The longest `Doc-Title` emitted, in characters. A header block is capped at
/// [`crate::http`]'s `MAX_HEADER` on the way in, and there is no reason for one
/// field to approach that.
const MAX_TITLE: usize = 200;

/// Which representation of a document was served.
///
/// This is part of the `ETag` because one URL yields three different byte
/// streams under content negotiation. A validator that ignored it would let a
/// cache answer a browser's request with the reply it stored for `curl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repr {
    /// The Markdown source, for `Accept: text/markdown`.
    Markdown,
    /// The reader-friendly text rendering, for terminals and `text/plain`.
    Text,
    /// The rendered page.
    Html,
    /// A static asset, streamed as it sits on disk.
    Raw,
}

impl Repr {
    fn tag(self) -> &'static str {
        match self {
            Repr::Markdown => "md",
            Repr::Text => "txt",
            Repr::Html => "html",
            Repr::Raw => "raw",
        }
    }
}

/// `Server`, which every response carries while the plugin is on.
pub fn server() -> (String, String) {
    (
        "Server".to_string(),
        format!("serve-md/{}", env!("CARGO_PKG_VERSION")),
    )
}

/// `Vary`, for the responses whose content was negotiated.
///
/// Both axes are real: `serve_file` branches on `Accept`, and on `User-Agent`
/// via `ua_is_terminal`. The listing does the same.
pub fn vary() -> (String, String) {
    ("Vary".to_string(), "Accept, User-Agent".to_string())
}

/// `Last-Modified` and `ETag` for one file in one representation.
///
/// Both are omitted rather than guessed when the platform cannot report a
/// modification time — a made-up validator is worse than none, since a client
/// would cache against it.
pub fn validators(meta: &Metadata, repr: Repr) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(m) = meta.modified() {
        out.push(("Last-Modified".to_string(), page::format_http_date(m)));
    }
    if let Some(tag) = etag(meta, repr) {
        out.push(("ETag".to_string(), tag));
    }
    out
}

/// The entity tag: modification time, size, and which representation.
///
/// Weak, because the rendered HTML also depends on the set of active plugins
/// and on serve-md's own version, so byte-for-byte equality is not promised
/// across a restart. What is promised is what weak comparison is defined to
/// mean — the same document, in the same representation.
pub fn etag(meta: &Metadata, repr: Repr) -> Option<String> {
    let secs = mtime_secs(meta)?;
    Some(format!("W/\"{secs}-{}-{}\"", meta.len(), repr.tag()))
}

/// `Link`: the document's own canonical path, plus the agent surface when
/// `webmcp` is also enabled.
///
/// One field with comma-separated values, per RFC 8288 §3. The `/llms.txt` and
/// `/mcp` entries are conditional because advertising routes this server is
/// not answering would be worse than saying nothing.
pub fn links(canonical: &str, mcp: bool) -> (String, String) {
    // Percent-encoded because the path comes from the request: a raw `>` or a
    // CR would otherwise end the field early or split it in two.
    let mut parts = vec![format!(
        "<{}>; rel=\"canonical\"",
        percent_encode_path(canonical)
    )];
    if mcp {
        parts.push("</llms.txt>; rel=\"alternate\"; type=\"text/plain\"".to_string());
        parts.push("</mcp>; rel=\"service-desc\"; type=\"application/json\"".to_string());
    }
    ("Link".to_string(), parts.join(", "))
}

/// The `Doc-*` set for a document whose metadata has been read.
pub fn document(kind: FileKind, brief: &Brief) -> Vec<(String, String)> {
    let mut out = vec![("Doc-Format".to_string(), format_name(kind).to_string())];
    if let Some(title) = &brief.title {
        let (ascii, ext) = sanitize_title(title);
        if !ascii.is_empty() {
            out.push(("Doc-Title".to_string(), ascii));
        }
        if let Some(ext) = ext {
            out.push(("Doc-Title*".to_string(), ext));
        }
    }
    out.push(("Doc-Words".to_string(), brief.words.to_string()));
    out.push(("Doc-Headings".to_string(), brief.headings.to_string()));
    out
}

/// `Doc-Format` for a file served without being parsed.
pub fn format_name(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Markdown => "markdown",
        FileKind::Html => "html",
        FileKind::Static => "static",
    }
}

/// A title as a header value: an ASCII form, and the RFC 8187 `ext-value`
/// beside it when the title was not ASCII to begin with.
///
/// Field values are ASCII (RFC 9110 §5.5), and a title is author-controlled
/// text that may hold anything — including a CR or LF, which would let a
/// heading inject a header of its own. Whitespace is collapsed first, which
/// removes that class of input entirely, then everything outside printable
/// ASCII is dropped.
///
/// Emitting both spellings rather than only the encoded one follows
/// `Content-Disposition`'s `filename` / `filename*` pairing, which is the
/// deployed precedent for this exact problem: a reader that knows nothing
/// about ext-values still gets something readable.
pub fn sanitize_title(title: &str) -> (String, Option<String>) {
    let collapsed = clip(&title.split_whitespace().collect::<Vec<_>>().join(" "), MAX_TITLE);
    let ascii: String = collapsed
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    let ext = if !collapsed.is_ascii() {
        Some(format!("UTF-8''{}", percent_encode_attr(&collapsed)))
    } else {
        None
    };
    (ascii.trim().to_string(), ext)
}

/// Whether a conditional request may be answered with 304.
///
/// `If-None-Match` wins outright when present, and `If-Modified-Since` is
/// consulted only in its absence — RFC 9110 §13.1.3 says a recipient MUST
/// ignore the date when the entity tag was sent, because the tag is the more
/// precise validator and the two can disagree.
///
/// Anything unrecognised answers `false`, which serves the full response. That
/// is always correct, only wasteful; the opposite mistake serves a stale body.
pub fn matches(
    if_none_match: Option<&str>,
    if_modified_since: Option<&str>,
    meta: &Metadata,
    repr: Repr,
) -> bool {
    if let Some(inm) = if_none_match {
        let Some(current) = etag(meta, repr) else {
            return false;
        };
        return inm
            .split(',')
            .map(str::trim)
            .any(|c| c == "*" || weak_eq(c, &current));
    }
    let Some(ims) = if_modified_since else {
        return false;
    };
    let (Some(since), Some(secs)) = (page::parse_http_date(ims), mtime_secs(meta)) else {
        return false;
    };
    // Whole-second granularity on both sides, so `<=` is the comparison that
    // means "not modified since".
    secs <= since
}

/// Borrows an owned header list for [`crate::http`]'s `&[(&str, &str)]` slot.
///
/// The headers here are built, not static, and every call site would otherwise
/// repeat the same `map`.
pub fn borrow(headers: &[(String, String)]) -> Vec<(&str, &str)> {
    headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect()
}

/// RFC 9110 §8.8.3.2 weak comparison: the `W/` marker is not part of the
/// value being compared.
fn weak_eq(a: &str, b: &str) -> bool {
    a.strip_prefix("W/").unwrap_or(a) == b.strip_prefix("W/").unwrap_or(b)
}

fn mtime_secs(meta: &Metadata) -> Option<u64> {
    meta.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Truncates to `max` characters, never mid-character.
fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Set;

    #[test]
    fn the_plugin_is_registered_and_described() {
        assert_eq!(XHeaders.name(), "x-headers");
        assert!(crate::plugin::catalog().iter().any(|l| l.starts_with("x-headers — ")));
        assert!(Set::resolve(&["x-headers".to_string()]).unwrap().has("x-headers"));
    }

    #[test]
    fn the_plugin_changes_no_markup() {
        // The contrast with every other plugin: enabling this one must leave
        // the rendered page byte-for-byte what it was.
        let with = Set::resolve(&["x-headers".to_string()]).unwrap();
        let without = Set::default();
        let src = "# Title\n\nSome $x$ prose.\n";
        assert_eq!(with.render_html(src).html, without.render_html(src).html);
        assert!(with.render_html(src).head.is_empty());
    }

    #[test]
    fn the_server_header_names_this_build() {
        let (name, value) = server();
        assert_eq!(name, "Server");
        assert_eq!(value, format!("serve-md/{}", env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn representations_get_different_etags() {
        // The whole reason Repr is in the tag: a cache keyed on the URL alone
        // must not be able to confuse these two.
        let meta = std::fs::metadata("Cargo.toml").unwrap();
        let html = etag(&meta, Repr::Html).unwrap();
        let md = etag(&meta, Repr::Markdown).unwrap();
        assert_ne!(html, md);
        assert!(html.starts_with("W/\""), "got: {html}");
    }

    #[test]
    fn titles_cannot_inject_a_header() {
        let (ascii, ext) = sanitize_title("Real Title\r\nX-Injected: yes");
        assert!(!ascii.contains('\r') && !ascii.contains('\n'));
        assert_eq!(ascii, "Real Title X-Injected: yes");
        assert!(ext.is_none());
    }

    #[test]
    fn non_ascii_titles_get_an_ext_value_beside_the_plain_one() {
        let (ascii, ext) = sanitize_title("Café Guide");
        // The plain form stays readable rather than becoming mojibake.
        assert_eq!(ascii, "Caf Guide");
        assert_eq!(ext.unwrap(), "UTF-8''Caf%C3%A9%20Guide");
    }

    #[test]
    fn long_titles_are_clipped_without_splitting_a_character() {
        let long = "é".repeat(MAX_TITLE * 2);
        let (_, ext) = sanitize_title(&long);
        // Clipping happens in characters, so the encoded form is exactly
        // MAX_TITLE two-byte characters and never a half of one.
        assert_eq!(ext.unwrap().len(), "UTF-8''".len() + MAX_TITLE * 6);
    }

    #[test]
    fn the_etag_beats_the_date_when_both_are_sent() {
        // RFC 9110 §13.1.3: a non-matching If-None-Match is the answer even
        // when the date alone would have said "not modified".
        let meta = std::fs::metadata("Cargo.toml").unwrap();
        let future = page::format_http_date(
            std::time::SystemTime::now() + std::time::Duration::from_secs(86_400),
        );
        assert!(!matches(Some("W/\"nope\""), Some(&future), &meta, Repr::Html));
        assert!(matches(None, Some(&future), &meta, Repr::Html));
    }

    #[test]
    fn a_matching_etag_is_recognised_strong_or_weak() {
        let meta = std::fs::metadata("Cargo.toml").unwrap();
        let tag = etag(&meta, Repr::Html).unwrap();
        let strong = tag.trim_start_matches("W/").to_string();
        assert!(matches(Some(&tag), None, &meta, Repr::Html));
        assert!(matches(Some(&strong), None, &meta, Repr::Html));
        assert!(matches(Some("*"), None, &meta, Repr::Html));
        assert!(matches(Some(&format!("W/\"other\", {tag}")), None, &meta, Repr::Html));
        // ...but not the tag for a different representation.
        assert!(!matches(Some(&tag), None, &meta, Repr::Markdown));
    }

    #[test]
    fn an_old_date_serves_the_body() {
        let meta = std::fs::metadata("Cargo.toml").unwrap();
        assert!(!matches(None, Some("Thu, 01 Jan 1970 00:00:00 GMT"), &meta, Repr::Html));
        assert!(!matches(None, Some("not a date"), &meta, Repr::Html));
        assert!(!matches(None, None, &meta, Repr::Html));
    }

    #[test]
    fn links_name_the_agent_surface_only_when_it_exists() {
        let (name, without) = links("/guides/start.md", false);
        assert_eq!(name, "Link");
        assert_eq!(without, "</guides/start.md>; rel=\"canonical\"");

        let (_, with) = links("/guides/start.md", true);
        assert!(with.contains("</llms.txt>; rel=\"alternate\""));
        assert!(with.contains("</mcp>; rel=\"service-desc\""));
    }

    #[test]
    fn link_paths_are_encoded() {
        // A raw space or `>` here would end the field early or split it.
        let (_, value) = links("/a b/c>d.md", false);
        assert_eq!(value, "</a%20b/c%3Ed.md>; rel=\"canonical\"");
    }

    #[test]
    fn the_doc_set_describes_the_document() {
        let brief = Brief {
            title: Some("Getting Started".to_string()),
            words: 812,
            headings: 14,
        };
        let out = document(FileKind::Markdown, &brief);
        let get = |k: &str| out.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("Doc-Format"), Some("markdown"));
        assert_eq!(get("Doc-Title"), Some("Getting Started"));
        assert_eq!(get("Doc-Words"), Some("812"));
        assert_eq!(get("Doc-Headings"), Some("14"));
        assert_eq!(get("Doc-Title*"), None);
    }

    #[test]
    fn an_untitled_document_omits_the_title_rather_than_faking_one() {
        let brief = Brief { title: None, words: 3, headings: 0 };
        let out = document(FileKind::Html, &brief);
        assert!(out.iter().all(|(n, _)| n != "Doc-Title"));
        assert!(out.iter().any(|(n, v)| n == "Doc-Format" && v == "html"));
    }
}

//! `/llms.txt` and `/llms-full.txt`, generated from the served tree.
//!
//! `llms.txt` is to language models what `robots.txt` is to crawlers: a file
//! at a well-known path that says what a site contains, in Markdown, so a
//! model does not have to infer structure from rendered HTML. serve-md already
//! knows every document it serves and can read each one's title and opening
//! sentence, so it can write the file itself — no build step, no committed
//! artefact to fall out of date.
//!
//! A local `llms.txt` always wins. If the author wrote one, that is the
//! answer; generation only fills the gap when they did not. The check lives in
//! `http::route`, which consults the catalog before calling this module.
//!
//! # Standard
//!
//! The /llms.txt file, v2 — <https://llmstxt.org/>
//!
//! The shape required there, in order: an H1 with the project name (the only
//! required part), an optional blockquote summary, zero or more Markdown
//! sections, then zero or more H2-delimited lists of links, each entry a
//! Markdown hyperlink with an optional `: note` after it. The `Optional`
//! section is reserved by convention for links an agent may skip when it needs
//! a shorter context.

use crate::catalog::Snapshot;
use crate::docmeta;
use crate::encoding::percent_encode_path;
use crate::scanner::FileKind;
use comrak::Options;
use std::fs;
use std::path::Path;

/// A ceiling on `/llms-full.txt`. Concatenating a whole documentation tree
/// into one response is the point of the file, but it must still be a bounded
/// one: without a cap, a directory of large Markdown files turns a single
/// unauthenticated GET into an arbitrarily large allocation.
const MAX_FULL_BYTES: usize = 5 * 1024 * 1024;

/// The documents worth naming to a model: things with prose in them. Static
/// assets are served, but listing a favicon helps nobody.
fn is_document(rel: &str) -> bool {
    matches!(
        FileKind::from_path(Path::new(rel)),
        FileKind::Markdown | FileKind::Html
    )
}

/// Root-level `README.md`, whatever its capitalisation — the conventional
/// place to find what a tree is about.
fn readme(snap: &Snapshot) -> Option<&str> {
    snap.files
        .iter()
        .map(|f| f.rel.as_str())
        .find(|rel| !rel.contains('/') && rel.eq_ignore_ascii_case("README.md"))
}

/// The H2 a document belongs under: its top-level directory, or `Docs` for a
/// file sitting at the root.
fn section_of(rel: &str) -> &str {
    match rel.split_once('/') {
        Some((dir, _)) => dir,
        None => "Docs",
    }
}

/// Builds `/llms.txt`.
///
/// `mcp` says whether the `webmcp` plugin is active, which decides only
/// whether the file mentions the `/mcp` endpoint — an agent that finds one
/// entry point should be told about the other.
pub fn llms_txt(root: &Path, snap: &Snapshot, options: &Options<'_>, mcp: bool) -> String {
    let readme_rel = readme(snap);
    let readme_src = readme_rel.and_then(|rel| read(root, rel));

    let name = readme_src
        .as_deref()
        .and_then(|src| docmeta::title(src, options))
        .unwrap_or_else(|| dir_name(root));
    let blurb = readme_src
        .as_deref()
        .and_then(|src| docmeta::summary(src, options));

    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", escape_inline(&name)));
    if let Some(b) = blurb {
        out.push_str(&format!("> {}\n\n", escape_inline(&b)));
    }

    // A free-text section, which the spec allows between the blockquote and
    // the first H2. It is the only place to tell an agent how to fetch these
    // documents efficiently.
    out.push_str("Every page below is also available as raw Markdown: request it with\n");
    out.push_str("`Accept: text/markdown`, or as plain text with `Accept: text/plain`.\n");
    if mcp {
        out.push_str(
            "\nThis site also speaks the Model Context Protocol at `/mcp`, where the\n\
             `search_docs`, `read_doc`, `list_docs` and `get_outline` tools operate over\n\
             exactly these documents. Prefer it to fetching pages one at a time.\n",
        );
    }
    out.push_str("\nThe full text of every document is at `/llms-full.txt`.\n");

    // Root-level documents first, then each directory alphabetically. The
    // catalog is already sorted by path, so within a section source order is
    // path order.
    let mut sections: Vec<&str> = Vec::new();
    for f in &snap.files {
        if !is_document(&f.rel) {
            continue;
        }
        let s = section_of(&f.rel);
        if !sections.contains(&s) {
            sections.push(s);
        }
    }
    sections.sort_by_key(|s| (*s != "Docs", *s));

    for section in sections {
        out.push_str(&format!("\n## {}\n\n", escape_inline(section)));
        for f in &snap.files {
            if !is_document(&f.rel) || section_of(&f.rel) != section {
                continue;
            }
            let src = read(root, &f.rel);
            let title = src
                .as_deref()
                .and_then(|s| docmeta::title(s, options))
                .unwrap_or_else(|| f.rel.clone());
            let note = src.as_deref().and_then(|s| docmeta::summary(s, options));
            let url = percent_encode_path(&format!("/{}", f.rel));
            match note {
                Some(n) => out.push_str(&format!(
                    "- [{}]({}): {}\n",
                    escape_inline(&title),
                    url,
                    escape_inline(&n)
                )),
                None => out.push_str(&format!("- [{}]({})\n", escape_inline(&title), url)),
            }
        }
    }
    out
}

/// Builds `/llms-full.txt`: every document, in catalog order, under a heading
/// naming its path.
pub fn llms_full_txt(root: &Path, snap: &Snapshot, options: &Options<'_>) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", escape_inline(&dir_name(root))));
    out.push_str("The full text of every document served here, concatenated.\n");

    let mut truncated = false;
    for f in &snap.files {
        if !is_document(&f.rel) {
            continue;
        }
        let Some(src) = read(root, &f.rel) else {
            continue;
        };
        let title = docmeta::title(&src, options).unwrap_or_else(|| f.rel.clone());
        let block = format!(
            "\n\n---\n\n## {}\n\nSource: `/{}`\n\n{}\n",
            escape_inline(&title),
            f.rel,
            src.trim_end()
        );
        if out.len() + block.len() > MAX_FULL_BYTES {
            truncated = true;
            break;
        }
        out.push_str(&block);
    }
    if truncated {
        out.push_str(
            "\n\n---\n\nTruncated: the remaining documents did not fit. \
             Fetch them individually, or use the `/mcp` endpoint.\n",
        );
    }
    out
}

fn read(root: &Path, rel: &str) -> Option<String> {
    fs::read_to_string(root.join(rel)).ok()
}

fn dir_name(root: &Path) -> String {
    root.canonicalize()
        .ok()
        .as_deref()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Documents".to_string())
}

/// Neutralises the few characters that would break the Markdown construct a
/// value is being placed into — a `]` ending a link text early, or a newline
/// ending a list item. The output is Markdown for a model to read, not HTML
/// for a browser to render, so this is about structure, not injection.
fn escape_inline(s: &str) -> String {
    s.replace('\\', r"\\")
        .replace('[', r"\[")
        .replace(']', r"\]")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::plugin::Set;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::path::PathBuf;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "serve-md-llms-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            tag
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample() -> PathBuf {
        let d = tmp("sample");
        fs::create_dir_all(d.join("guides")).unwrap();
        fs::write(
            d.join("README.md"),
            "# Widget Docs\n\nEverything about the widget pipeline. More detail follows.\n",
        )
        .unwrap();
        fs::write(
            d.join("guides/start.md"),
            "# Getting Started\n\nInstall the CLI and render your first document.\n",
        )
        .unwrap();
        fs::write(d.join("guides/security.md"), "# Security\n\nBasic auth and paths.\n").unwrap();
        fs::write(d.join("logo.png"), "notreallyapng").unwrap();
        d
    }

    fn generate(d: &Path, mcp: bool) -> String {
        let snap = Catalog::scan(d).unwrap().current();
        llms_txt(d, &snap, &Set::default().options(), mcp)
    }

    #[test]
    fn follows_the_required_order_from_the_spec() {
        let d = sample();
        let out = generate(&d, false);
        let h1 = out.find("# Widget Docs").unwrap();
        let quote = out.find("> Everything about the widget pipeline.").unwrap();
        let first_h2 = out.find("\n## ").unwrap();
        assert!(h1 < quote, "H1 precedes the blockquote");
        assert!(quote < first_h2, "the blockquote precedes the first H2");
        assert!(out.starts_with("# Widget Docs\n\n"));
    }

    #[test]
    fn titles_and_summaries_come_from_the_documents() {
        let out = generate(&sample(), false);
        assert!(out.contains(
            "- [Getting Started](/guides/start.md): Install the CLI and render your first document."
        ));
        assert!(out.contains("- [Security](/guides/security.md): Basic auth and paths."));
    }

    #[test]
    fn root_files_come_first_then_directories() {
        let out = generate(&sample(), false);
        let docs = out.find("## Docs").unwrap();
        let guides = out.find("## guides").unwrap();
        assert!(docs < guides);
    }

    #[test]
    fn static_assets_are_not_listed() {
        let out = generate(&sample(), false);
        assert!(!out.contains("logo.png"));
    }

    #[test]
    fn the_mcp_endpoint_is_announced_only_when_enabled() {
        let d = sample();
        assert!(!generate(&d, false).contains("/mcp"));
        let on = generate(&d, true);
        assert!(on.contains("Model Context Protocol at `/mcp`"));
        assert!(on.contains("search_docs"));
    }

    #[test]
    fn a_tree_without_a_readme_falls_back_to_the_directory_name() {
        let d = tmp("noreadme");
        fs::write(d.join("a.md"), "# A\n\nText.\n").unwrap();
        let out = generate(&d, false);
        let expected = d.canonicalize().unwrap();
        let expected = expected.file_name().unwrap().to_string_lossy();
        assert!(out.starts_with(&format!("# {expected}\n")), "got: {out}");
    }

    #[test]
    fn a_document_without_a_heading_falls_back_to_its_path() {
        let d = tmp("noheading");
        fs::write(d.join("plain.md"), "just prose, no heading at all.\n").unwrap();
        let out = generate(&d, false);
        assert!(out.contains("- [plain.md](/plain.md): just prose, no heading at all."));
    }

    #[test]
    fn paths_with_spaces_are_percent_encoded() {
        let d = tmp("spaces");
        fs::write(d.join("my file.md"), "# My File\n\nHi.\n").unwrap();
        let out = generate(&d, false);
        assert!(out.contains("(/my%20file.md)"), "got: {out}");
    }

    #[test]
    fn brackets_in_a_title_cannot_break_the_link() {
        let d = tmp("brackets");
        fs::write(d.join("a.md"), "# The [weird] one\n\nHi.\n").unwrap();
        let out = generate(&d, false);
        assert!(out.contains(r"- [The \[weird\] one](/a.md)"), "got: {out}");
    }

    #[test]
    fn full_text_contains_every_document_and_its_source_path() {
        let d = sample();
        let snap = Catalog::scan(&d).unwrap().current();
        let out = llms_full_txt(&d, &snap, &Set::default().options());
        assert!(out.contains("Source: `/guides/start.md`"));
        assert!(out.contains("Install the CLI and render your first document."));
        assert!(out.contains("Source: `/README.md`"));
        assert!(!out.contains("logo.png"));
    }

    #[test]
    fn full_text_is_bounded() {
        let d = tmp("big");
        let big = "x".repeat(600 * 1024);
        for i in 0..20 {
            fs::write(d.join(format!("{i:02}.md")), &big).unwrap();
        }
        let snap = Catalog::scan(&d).unwrap().current();
        let out = llms_full_txt(&d, &snap, &Set::default().options());
        assert!(out.len() <= MAX_FULL_BYTES + 1024, "got {} bytes", out.len());
        assert!(out.contains("Truncated:"));
    }

    #[test]
    fn an_empty_tree_still_produces_a_valid_file() {
        let d = tmp("empty");
        let out = generate(&d, false);
        assert!(out.starts_with("# "));
        assert!(!out.contains("## "));
    }
}
